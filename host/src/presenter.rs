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
    params: wgpu::Buffer,
    bindings: wgpu::BindGroup,
}

impl UploadedFrame {
    fn new(
        device: &wgpu::Device,
        frame_layout: &wgpu::BindGroupLayout,
        size: wgpu::Extent3d,
        srgb: bool,
    ) -> Self {
        // The game's bytes are already display-encoded. An sRGB texture decodes
        // them to linear on read, and the sRGB swapchain view re-encodes them on
        // write, so the exact same bytes reach the screen. Using a non-sRGB
        // texture skipped the decode and the write applied a second sRGB
        // encode, which is why the picture came out washed out.
        let raw_format = if srgb {
            wgpu::TextureFormat::Rgba8UnormSrgb
        } else {
            wgpu::TextureFormat::Rgba8Unorm
        };
        let raw_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("DirectDraw RGBA primary"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: raw_format,
            usage: wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let params = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("DirectDraw frame params"),
            size: 32,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let raw_view = raw_texture.create_view(&Default::default());
        let bindings = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("DirectDraw frame bindings"),
            layout: frame_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&raw_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: params.as_entire_binding(),
                },
            ],
        });
        Self {
            raw_texture,
            params,
            bindings,
        }
    }
}

fn frame_bind_group_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("DirectDraw frame layout"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: wgpu::BufferSize::new(32),
                },
                count: None,
            },
        ],
    })
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
    frame_layout: wgpu::BindGroupLayout,
    pipeline: wgpu::RenderPipeline,
    const_pipeline: wgpu::RenderPipeline,
    /// `fs_const` driven through an empty pipeline layout (no bind group at all).
    const_nobind_pipeline: wgpu::RenderPipeline,
    solid_pipeline: wgpu::RenderPipeline,
    frame: Option<UploadedFrame>,
    staging: Vec<u8>,
    frame_dims: (u32, u32),
    /// CPU nearest-neighbour upscale of the frame, already in the target's
    /// channel order (`Bgra8`).
    scaled: Vec<u8>,
    /// Destination-x -> source-x lookup for the current viewport.
    xmap: Vec<u32>,
    /// Target-sized region uploaded by the blit path.
    blit_texture: Option<wgpu::Texture>,
    blit: bool,
    /// A new frame has been staged and the blit source must be rebuilt.
    dirty: bool,
    maintas: bool,
    clear_color: wgpu::Color,
    clear_only: bool,
    solid: bool,
    solid_draw: bool,
    const_fs: bool,
    const_nobind: bool,
    full_viewport: bool,
    logged_frame: bool,
    logged_present: bool,
    probed: bool,
    frame_count: u64,
    present_count: u64,
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
        // The default present path blits the frame into the swapchain image with
        // a texture copy, so it must be a copy destination.
        surface_cfg.usage =
            wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_DST;

        // One explicit layout shared by every pipeline. Pipeline-derived
        // ("exclusive") layouts are not interchangeable in wgpu 27, which is
        // why the previous diagnostic pipeline could never bind the frame
        // bind group at all.
        let frame_layout = frame_bind_group_layout(&device);
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("DirectDraw pipeline layout"),
            bind_group_layouts: &[&frame_layout],
            push_constant_ranges: &[],
        });

        // Layout with no bind groups at all: used by the two diagnostics that
        // must draw without any descriptor binding.
        let empty_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("DirectDraw empty layout"),
            bind_group_layouts: &[],
            push_constant_ranges: &[],
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("DirectDraw RGBA present"),
            source: wgpu::ShaderSource::Wgsl(include_str!("present.wgsl").into()),
        });
        let make_pipeline = |label: &str, entry: &str| {
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&pipeline_layout),
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
                    entry_point: Some(entry),
                    compilation_options: Default::default(),
                    targets: &[Some(surface_cfg.format.into())],
                }),
                multiview: None,
                cache: None,
            })
        };
        let pipeline = make_pipeline("DirectDraw presentation", "fs");
        let const_pipeline = make_pipeline("DirectDraw const diagnostic", "fs_const");
        let const_nobind_pipeline =
            device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
                label: Some("DirectDraw const no-bind diagnostic"),
                layout: Some(&empty_layout),
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
                    entry_point: Some("fs_const"),
                    compilation_options: Default::default(),
                    targets: &[Some(surface_cfg.format.into())],
                }),
                multiview: None,
                cache: None,
            });

        let solid_shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("DirectDraw solid diagnostic"),
            source: wgpu::ShaderSource::Wgsl(include_str!("solid.wgsl").into()),
        });
        // This path deliberately draws without any bind group, so it must not
        // declare bind group 0 (wgpu rejects a draw that leaves a declared
        // group unset).
        let solid_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("DirectDraw solid diagnostic"),
            layout: Some(&empty_layout),
            vertex: wgpu::VertexState {
                module: &solid_shader,
                entry_point: Some("vs"),
                buffers: &[],
                compilation_options: Default::default(),
            },
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            fragment: Some(wgpu::FragmentState {
                module: &solid_shader,
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
        let solid_draw = std::env::var_os("CNC_DDRAW_HOST_SOLID_DRAW").is_some();
        let const_fs = std::env::var_os("CNC_DDRAW_HOST_CONST_FS").is_some();
        let const_nobind = std::env::var_os("CNC_DDRAW_HOST_CONST_NOBIND").is_some();
        let full_viewport = std::env::var_os("CNC_DDRAW_HOST_FULL_VIEWPORT").is_some();
        // The render-pipeline path is the default: the GPU does the scaling and
        // the CPU only converts the frame. `CNC_DDRAW_HOST_BLIT=1` falls back to
        // the CPU nearest-neighbour upscale plus `copy_texture_to_texture`, which
        // needs no pipeline at all.
        let blit = std::env::var_os("CNC_DDRAW_HOST_BLIT").is_some();
        logging::log_msg(&format!(
            "presenter init: format={:?} blit={blit} clear_only={clear_only} solid={solid} solid_draw={solid_draw} const_fs={const_fs} const_nobind={const_nobind} full_viewport={full_viewport}",
            surface_cfg.format
        ));

        Ok(Self {
            surface,
            device,
            queue,
            config: surface_cfg,
            configured: false,
            frame_layout,
            pipeline,
            const_pipeline,
            const_nobind_pipeline,
            solid_pipeline,
            frame: None,
            staging: Vec::new(),
            frame_dims: (0, 0),
            scaled: Vec::new(),
            xmap: Vec::new(),
            blit_texture: None,
            blit,
            dirty: false,
            maintas,
            clear_color,
            clear_only,
            solid,
            solid_draw,
            const_fs,
            const_nobind,
            full_viewport,
            logged_frame: false,
            logged_present: false,
            probed: false,
            frame_count: 0,
            present_count: 0,
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
        self.frame_dims = (width, height);
        self.dirty = true;
        if self.blit {
            // The copy path scales on the CPU and never needs a GPU frame
            // texture.
            self.frame = None;
        } else {
            let srgb = self.config.format.is_srgb();
            if self.frame.as_ref().is_some_and(|frame| {
                frame.raw_texture.size() != size
                    || frame.raw_texture.format().is_srgb() != srgb
            }) {
                self.frame = None;
            }
            if self.frame.is_none() {
                self.frame = Some(UploadedFrame::new(
                    &self.device,
                    &self.frame_layout,
                    size,
                    srgb,
                ));
            }
        }

        // Match the original in-process renderer: queue.write_texture accepts
        // a tightly packed row stride (the original passes the DirectDraw pitch
        // directly, e.g. 1600 for an 800-pixel RGB565 surface). Do not invent a
        // 256-byte-aligned stride here; keep the upload byte-for-byte simple.
        let out_pitch = width
            .checked_mul(4)
            .ok_or_else(|| "rgba pitch overflow".to_string())? as usize;
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
        }

        if self.frame_count <= 5 || self.frame_count % 120 == 0 {
            logging::log_msg(&format!(
                "staging #{}: head={:02x?}",
                self.frame_count,
                &self.staging[..self.staging.len().min(16)]
            ));
        }
        if !self.blit {
            let frame = self.frame.as_ref().expect("frame was just created");
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
        }
        Ok(())
    }

    pub fn present(&mut self, target_w: u32, target_h: u32) -> Result<(), String> {
        if target_w == 0 || target_h == 0 {
            return Ok(());
        }
        if !self.configured || self.config.width != target_w || self.config.height != target_h {
            self.config.width = target_w;
            self.config.height = target_h;
            if !self.configured {
                logging::log_msg(&format!(
                    "present: configuring swapchain {target_w}x{target_h}"
                ));
            }
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

        if self.blit {
            return self.present_blit(output, target_w, target_h);
        }

        let frame = self.frame.as_ref();
        if !self.clear_only && !self.solid_draw && !self.const_nobind && frame.is_none() {
            return Ok(());
        }
        if !self.logged_present {
            self.logged_present = true;
            logging::log_msg(&format!(
                "first present: target={target_w}x{target_h} frame={:?}",
                frame.map(|f| f.raw_texture.size())
            ));
        }
        let (vx, vy, vw, vh) = if self.full_viewport {
            (0, 0, target_w.max(1), target_h.max(1))
        } else if let Some(frame) = frame {
            let frame_size = frame.raw_texture.size();
            compute_viewport(
                frame_size.width,
                frame_size.height,
                target_w,
                target_h,
                self.maintas,
            )
        } else {
            (0, 0, target_w.max(1), target_h.max(1))
        };
        if let Some(frame) = frame {
            write_frame_params(&self.queue, frame, vx, vy, vw, vh);
            if !self.probed {
                self.probed = true;
                self.readback_probe(frame);
            }
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
            pass.set_viewport(vx as f32, vy as f32, vw as f32, vh as f32, 0.0, 1.0);
            if self.clear_only {
                // The swapchain is left as the cleared colour on purpose.
            } else if self.solid_draw {
                pass.set_pipeline(&self.solid_pipeline);
                pass.draw(0..3, 0..1);
            } else if self.const_nobind {
                pass.set_pipeline(&self.const_nobind_pipeline);
                pass.draw(0..3, 0..1);
            } else if let Some(frame) = frame {
                let selected = if self.const_fs {
                    &self.const_pipeline
                } else {
                    &self.pipeline
                };
                pass.set_pipeline(selected);
                pass.set_bind_group(0, &frame.bindings, &[]);
                pass.draw(0..3, 0..1);
            }
        }
        self.queue.submit([encoder.finish()]);
        output.present();
        self.present_count = self.present_count.wrapping_add(1);
        if self.present_count % 120 == 0 {
            logging::log_msg(&format!(
                "present #{}: target={target_w}x{target_h}",
                self.present_count
            ));
        }
        Ok(())
    }

    /// Portable present path.
    ///
    /// The frame is upscaled on the CPU and copied into the swapchain image
    /// with a plain texture copy. No render pipeline, shader, bind group,
    /// sampler or viewport is involved, only `write_texture` and
    /// `copy_texture_to_texture` - the two operations the readback probe has
    /// already proven to work on this stack.
    fn present_blit(
        &mut self,
        output: wgpu::SurfaceTexture,
        target_w: u32,
        target_h: u32,
    ) -> Result<(), String> {
        let (fw, fh) = self.frame_dims;
        if !self.clear_only && (fw == 0 || fh == 0) {
            return Ok(());
        }
        let (vx, vy, vw, vh) = if self.full_viewport || self.clear_only {
            (0, 0, target_w.max(1), target_h.max(1))
        } else {
            compute_viewport(fw, fh, target_w, target_h, self.maintas)
        };
        if !self.logged_present {
            self.logged_present = true;
            logging::log_msg(&format!(
                "first present (blit): target={target_w}x{target_h} frame={fw}x{fh} viewport={vx},{vy},{vw},{vh} format={:?}",
                self.config.format
            ));
        }

        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            // Deterministic letterbox bars.
            let view = output.texture.create_view(&Default::default());
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("DirectDraw blit clear"),
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
        }

        if !self.clear_only {
            let stride = (vw as usize * 4).div_ceil(256) * 256;
            let size = wgpu::Extent3d {
                width: vw,
                height: vh,
                depth_or_array_layers: 1,
            };
            // `present` runs every few milliseconds while the game may only
            // produce 15-30 frames per second, so only rescale and re-upload
            // when a new frame (or a new viewport size) actually arrived.
            let stale = self.dirty
                || !self
                    .blit_texture
                    .as_ref()
                    .is_some_and(|texture| texture.size() == size);
            if stale {
                let bgra = self.config.format.remove_srgb_suffix()
                    == wgpu::TextureFormat::Bgra8Unorm;
                self.build_scaled(fw, fh, vw, vh, bgra);
                if !self
                    .blit_texture
                    .as_ref()
                    .is_some_and(|texture| texture.size() == size)
                {
                    self.blit_texture =
                        Some(self.device.create_texture(&wgpu::TextureDescriptor {
                            label: Some("DirectDraw blit source"),
                            size,
                            mip_level_count: 1,
                            sample_count: 1,
                            dimension: wgpu::TextureDimension::D2,
                            // Match the swapchain's real (non-sRGB) DXGI
                            // resource so the copy needs no format conversion.
                            format: self.config.format.remove_srgb_suffix(),
                            usage: wgpu::TextureUsages::COPY_DST
                                | wgpu::TextureUsages::COPY_SRC,
                            view_formats: &[],
                        }));
                }
                let blit_texture = self.blit_texture.as_ref().expect("blit texture");
                self.queue.write_texture(
                    blit_texture.as_image_copy(),
                    &self.scaled[..stride * vh as usize],
                    wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(stride as u32),
                        rows_per_image: None,
                    },
                    size,
                );
                self.dirty = false;
            }
            let blit_texture = self.blit_texture.as_ref().expect("blit texture");
            encoder.copy_texture_to_texture(
                blit_texture.as_image_copy(),
                wgpu::TexelCopyTextureInfo {
                    texture: &output.texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d {
                        x: vx,
                        y: vy,
                        z: 0,
                    },
                    aspect: wgpu::TextureAspect::All,
                },
                size,
            );
        }

        self.queue.submit([encoder.finish()]);
        output.present();
        self.present_count = self.present_count.wrapping_add(1);
        if self.present_count % 120 == 0 {
            logging::log_msg(&format!(
                "present #{}: target={target_w}x{target_h} (blit)",
                self.present_count
            ));
        }
        Ok(())
    }

    /// Nearest-neighbour upscale of the staged RGBA frame into the target
    /// region, writing the swapchain's channel order.
    fn build_scaled(&mut self, fw: u32, fh: u32, vw: u32, vh: u32, bgra: bool) {
        let sw = fw as usize;
        let sh = fh as usize;
        let dw = vw as usize;
        let dh = vh as usize;
        if sw == 0 || sh == 0 || dw == 0 || dh == 0 {
            return;
        }
        let stride = (dw * 4).div_ceil(256) * 256;
        let needed = stride * dh;
        if self.scaled.len() < needed {
            self.scaled.resize(needed, 0);
        }
        if self.xmap.len() != dw {
            self.xmap.resize(dw, 0);
        }
        for (dx, slot) in self.xmap.iter_mut().enumerate() {
            *slot = ((dx as u64 * sw as u64) / dw as u64) as u32;
        }

        for dy in 0..dh {
            let sy = ((dy as u64 * sh as u64) / dh as u64) as usize;
            let src_row = &self.staging[sy * sw * 4..(sy + 1) * sw * 4];
            let dst_row = &mut self.scaled[dy * stride..dy * stride + dw * 4];
            for (dx, slot) in dst_row.chunks_exact_mut(4).enumerate() {
                let sx = self.xmap[dx] as usize * 4;
                let pixel = &src_row[sx..sx + 4];
                if bgra {
                    slot[0] = pixel[2];
                    slot[1] = pixel[1];
                    slot[2] = pixel[0];
                } else {
                    slot[0] = pixel[0];
                    slot[1] = pixel[1];
                    slot[2] = pixel[2];
                }
                slot[3] = 255;
            }
        }
    }

    fn readback_probe(&self, frame: &UploadedFrame) {
        let width = frame.raw_texture.width().max(1);
        let row_bytes = width.saturating_mul(4).max(4);
        let padded = row_bytes.div_ceil(256) * 256;
        let buffer = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("DirectDraw readback"),
            size: u64::from(padded),
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let mut encoder = self.device.create_command_encoder(&Default::default());
        encoder.copy_texture_to_buffer(
            frame.raw_texture.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(1),
                },
            },
            wgpu::Extent3d {
                width,
                height: 1,
                depth_or_array_layers: 1,
            },
        );
        self.queue.submit([encoder.finish()]);

        let slice = buffer.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
        // Bounded wait: a diagnostic must never freeze the presenter.
        let timeout = std::time::Duration::from_millis(1000);
        let _ = self.device.poll(wgpu::PollType::Wait {
            submission_index: None,
            timeout: Some(timeout),
        });
        match rx.recv_timeout(timeout) {
            Ok(Ok(())) => {
                let data = slice.get_mapped_range();
                logging::log_msg(&format!(
                    "gpu row0: {:02x?}",
                    &data[..data.len().min(16)]
                ));
                drop(data);
            }
            Ok(Err(err)) => logging::log_msg(&format!("gpu readback failed: {err}")),
            Err(err) => logging::log_msg(&format!("gpu readback unavailable: {err}")),
        }
        buffer.unmap();
    }
}

fn write_frame_params(
    queue: &wgpu::Queue,
    frame: &UploadedFrame,
    origin_x: u32,
    origin_y: u32,
    target_w: u32,
    target_h: u32,
) {
    let size = frame.raw_texture.size();
    let mut bytes = [0u8; 32];
    bytes[0..4].copy_from_slice(&(size.width as f32).to_le_bytes());
    bytes[4..8].copy_from_slice(&(size.height as f32).to_le_bytes());
    bytes[8..12].copy_from_slice(&(origin_x as f32).to_le_bytes());
    bytes[12..16].copy_from_slice(&(origin_y as f32).to_le_bytes());
    bytes[16..20].copy_from_slice(&(target_w.max(1) as f32).to_le_bytes());
    bytes[20..24].copy_from_slice(&(target_h.max(1) as f32).to_le_bytes());
    queue.write_buffer(&frame.params, 0, &bytes);
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
