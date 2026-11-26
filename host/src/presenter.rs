//! 64-bit wgpu DX12 presenter. This is the host-side twin of the old in-process
//! renderer: it uploads raw primary-surface bytes from shared memory, unpacks
//! them in WGSL, and presents through the 64-bit host overlay window (D3DMetal
//! on macOS Wine).
use std::num::NonZeroIsize;
use std::sync::Arc;

use raw_window_handle::{Win32WindowHandle, WindowsDisplayHandle};
use windows::Win32::Foundation::{HWND, HINSTANCE};

use ddraw_protocol as protocol;
use protocol::{FrameFormat, Header};

use crate::logging;

fn raw_texture_format(format: FrameFormat) -> wgpu::TextureFormat {
    match format {
        FrameFormat::Indexed => wgpu::TextureFormat::R8Uint,
        FrameFormat::Rgb555 | FrameFormat::Rgb565 => wgpu::TextureFormat::R16Uint,
        FrameFormat::Xrgb => wgpu::TextureFormat::R32Uint,
    }
}

fn params_bytes(format: FrameFormat) -> [u8; 16] {
    let mut out = [0u8; 16];
    out[..4].copy_from_slice(&(format as u32).to_le_bytes());
    out
}

fn align_up(value: u32, alignment: u32) -> u32 {
    (value + alignment - 1) & !(alignment - 1)
}

struct UploadedFrame {
    raw_texture: wgpu::Texture,
    raw_format: wgpu::TextureFormat,
    lut_texture: wgpu::Texture,
    params_buffer: wgpu::Buffer,
    bindings: wgpu::BindGroup,
    last_format: Option<FrameFormat>,
    last_lut_gen: Option<u32>,
}

impl UploadedFrame {
    fn new(
        device: &wgpu::Device,
        pipeline: &wgpu::RenderPipeline,
        target_format: wgpu::TextureFormat,
        size: wgpu::Extent3d,
        raw_format: wgpu::TextureFormat,
    ) -> Self {
        let raw_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("DirectDraw raw primary pixels"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: raw_format,
            usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let lut_format = if target_format.is_srgb() {
            wgpu::TextureFormat::Rgba8UnormSrgb
        } else {
            wgpu::TextureFormat::Rgba8Unorm
        };
        let lut_texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("DirectDraw palette/gamma LUT"),
            size: wgpu::Extent3d {
                width: 256,
                height: 1,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: lut_format,
            usage: wgpu::TextureUsages::COPY_DST | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let params_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("DirectDraw present params"),
            size: 16,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let raw_view = raw_texture.create_view(&Default::default());
        let lut_view = lut_texture.create_view(&Default::default());
        let bindings = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("DirectDraw frame bindings"),
            layout: &pipeline.get_bind_group_layout(0),
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&raw_view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&lut_view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: params_buffer.as_entire_binding(),
                },
            ],
        });
        Self {
            raw_texture,
            raw_format,
            lut_texture,
            params_buffer,
            bindings,
            last_format: None,
            last_lut_gen: None,
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
        // SAFETY: `Overlay` outlives `Presenter` (field order in `App`), so the
        // raw HWND stays valid for the whole surface lifetime.
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
        // Wine DXGI has incomplete Mailbox/Immediate support. Fifo is the only
        // present mode we trust for D3DMetal and for pacing the 32-bit game.
        surface_cfg.present_mode = wgpu::PresentMode::Fifo;

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("DirectDraw GPU unpack and blit"),
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
        })
    }

    pub fn upload(&mut self, header: &Header, base: *const u8) -> Result<(), String> {
        let format = FrameFormat::from_u32(header.format).ok_or("unknown frame format")?;
        let width = header.width;
        let height = header.height;
        let pitch = header.pitch;
        if width == 0 || height == 0 || width > protocol::MAX_WIDTH || height > protocol::MAX_HEIGHT
        {
            return Err(format!("unsupported frame size {width}x{height}"));
        }
        let bpp = format.bytes_per_pixel();
        let min_pitch = width
            .checked_mul(bpp)
            .ok_or_else(|| "frame pitch overflow".to_string())?;
        if pitch < min_pitch
            || u64::from(pitch) * u64::from(height) > u64::from(header.slot_bytes)
        {
            return Err("invalid frame pitch/length".to_string());
        }

        let size = wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        };
        let raw_format = raw_texture_format(format);
        if self
            .frame
            .as_ref()
            .is_some_and(|frame| frame.raw_texture.size() != size || frame.raw_format != raw_format)
        {
            self.frame = None;
        }
        let frame = self.frame.get_or_insert_with(|| {
            UploadedFrame::new(
                &self.device,
                &self.pipeline,
                self.config.format,
                size,
                raw_format,
            )
        });

        if frame.last_format != Some(format) {
            self.queue
                .write_buffer(&frame.params_buffer, 0, &params_bytes(format));
            frame.last_format = Some(format);
        }
        if frame.last_lut_gen != Some(header.lut_gen) {
            let lut_bytes: &[u8] = header.lut.as_flattened();
            self.queue.write_texture(
                frame.lut_texture.as_image_copy(),
                lut_bytes,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(256 * 4),
                    rows_per_image: None,
                },
                wgpu::Extent3d {
                    width: 256,
                    height: 1,
                    depth_or_array_layers: 1,
                },
            );
            frame.last_lut_gen = Some(header.lut_gen);
        }

        // wgpu requires a 256-byte row alignment for texture uploads. Re-pack
        // rows into a host-owned staging buffer so odd 8-bit pitches work.
        let aligned_pitch = align_up(pitch, wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) as usize;
        let needed = aligned_pitch
            .checked_mul(height as usize)
            .ok_or_else(|| "staging size overflow".to_string())?;
        if self.staging.len() < needed {
            self.staging.resize(needed, 0);
        }
        for row in 0..height as usize {
            let src = unsafe { base.add(row * pitch as usize) };
            let dst = &mut self.staging[row * aligned_pitch..(row + 1) * aligned_pitch];
            // SAFETY: The producer guarantees pitch * height bytes at `base`.
            unsafe {
                std::ptr::copy_nonoverlapping(src, dst.as_mut_ptr(), pitch as usize);
            }
            dst[pitch as usize..].fill(0);
        }

        self.queue.write_texture(
            frame.raw_texture.as_image_copy(),
            &self.staging[..needed],
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(aligned_pitch as u32),
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
        if !self.configured
            || self.config.width != target_w
            || self.config.height != target_h
        {
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

        let Some(frame) = self.frame.as_ref() else {
            return Ok(());
        };
        let frame_size = frame.raw_texture.size();
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
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            let (vx, vy, vw, vh) = compute_viewport(
                frame_size.width,
                frame_size.height,
                target_w,
                target_h,
                self.maintas,
            );
            pass.set_viewport(vx as f32, vy as f32, vw as f32, vh as f32, 0.0, 1.0);
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &frame.bindings, &[]);
            pass.draw(0..3, 0..1);
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
