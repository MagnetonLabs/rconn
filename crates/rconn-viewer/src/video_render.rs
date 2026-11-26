//! Puts decoded video on screen with wgpu, without converting pixels on the CPU.
//!
//! 1. The decoder's YUV planes are uploaded as they are: Y as an `R8Unorm` texture, the
//!    chroma as one `Rg8Unorm` (NV12) or two `R8Unorm` (I420) textures. 1.5 bytes per
//!    pixel instead of RGBA's 4.
//! 2. A shader converts them into a persistent RGBA **frame texture** at the remote
//!    screen's size, with the stream's own colour matrix and range ([`Colour`]). This runs
//!    once per decoded picture, not per repaint.
//! 3. An egui paint callback draws the frame texture into the picture's rectangle
//!    (bilinear scaling).
//!
//! The frame texture is the viewer's framebuffer: it keeps the last picture, and can be
//! copied from and into (`COPY_SRC | COPY_DST`), so operations on the remote screen that
//! are not a new picture (for example moving a rectangle of it) can be applied to it in
//! [`Tab::update`] after (or instead of) the conversion.
//!
//! The GPU objects live in egui-wgpu's callback resources ([`install`] puts them there);
//! each tab owns a [`VideoView`], and its textures are freed once the tab is gone.

use eframe::egui_wgpu::{self, CallbackResources, CallbackTrait, ScreenDescriptor, wgpu};
use rconn_media::sps::Colour;
use rconn_media::video::{DecodedFrame, PixelLayout};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Instant;

/// The frame texture's format (gamma-encoded RGB, as egui expects for a gamma framebuffer).
pub const FRAME_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8Unorm;

const FULLSCREEN_VS: &str = r"
struct VsOut {
    @builtin(position) pos: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

// One triangle covering the viewport; uv 0..1 over it (y down).
@vertex
fn vs_main(@builtin(vertex_index) i: u32) -> VsOut {
    let uv = vec2<f32>(f32((i << 1u) & 2u), f32(i & 2u));
    var out: VsOut;
    out.pos = vec4<f32>(uv.x * 2.0 - 1.0, 1.0 - uv.y * 2.0, 0.0, 1.0);
    out.uv = uv;
    return out;
}
";

/// YUV planes -> RGB, one output pixel per frame-texture pixel (chroma: nearest sample,
/// as the CPU conversion does).
const CONVERT_FS: &str = r"
struct Params {
    r: vec4<f32>,
    g: vec4<f32>,
    b: vec4<f32>,
    // x: 1 for I420 (Cb, Cr in two planes), 0 for NV12 (one interleaved plane).
    flags: vec4<f32>,
};

@group(0) @binding(0) var y_plane: texture_2d<f32>;
@group(0) @binding(1) var c1_plane: texture_2d<f32>;
@group(0) @binding(2) var c2_plane: texture_2d<f32>;
@group(0) @binding(3) var<uniform> p: Params;

@fragment
fn fs_convert(in: VsOut) -> @location(0) vec4<f32> {
    let px = vec2<i32>(in.pos.xy);
    let c = px / 2;
    let luma = textureLoad(y_plane, px, 0).r;
    var chroma = textureLoad(c1_plane, c, 0).rg;
    if p.flags.x > 0.5 {
        chroma = vec2<f32>(chroma.x, textureLoad(c2_plane, c, 0).r);
    }
    let s = vec4<f32>(luma, chroma, 1.0);
    let rgb = vec3<f32>(dot(p.r, s), dot(p.g, s), dot(p.b, s));
    return vec4<f32>(clamp(rgb, vec3<f32>(0.0), vec3<f32>(1.0)), 1.0);
}
";

/// Frame texture -> egui's render target.
const DISPLAY_FS: &str = r"
@group(0) @binding(0) var frame: texture_2d<f32>;
@group(0) @binding(1) var frame_sampler: sampler;

@fragment
fn fs_gamma(in: VsOut) -> @location(0) vec4<f32> {
    return textureSample(frame, frame_sampler, in.uv);
}

// For an sRGB render target the GPU encodes on write: hand it linear values.
@fragment
fn fs_linear(in: VsOut) -> @location(0) vec4<f32> {
    let c = textureSample(frame, frame_sampler, in.uv);
    let cutoff = c.rgb < vec3<f32>(0.04045);
    let lower = c.rgb / vec3<f32>(12.92);
    let higher = pow((c.rgb + vec3<f32>(0.055)) / vec3<f32>(1.055), vec3<f32>(2.4));
    return vec4<f32>(select(higher, lower, cutoff), c.a);
}
";

/// The per-tab handle on the UI side: hand it pictures, paint it.
pub struct VideoView {
    slot: Arc<Slot>,
}

/// Shared between a tab and its GPU state.
#[derive(Default)]
struct Slot {
    /// The newest picture not yet on the GPU (latest wins).
    pending: Mutex<Option<Arc<DecodedFrame>>>,
    /// CPU time of the last upload + conversion, in microseconds.
    upload_us: AtomicU32,
}

impl Default for VideoView {
    fn default() -> Self {
        Self::new()
    }
}

impl VideoView {
    pub fn new() -> Self {
        Self {
            slot: Arc::default(),
        }
    }

    /// Show this picture from the next repaint on (replacing one not yet shown).
    pub fn submit(&self, frame: Arc<DecodedFrame>) {
        *self.slot.pending.lock().expect("video slot") = Some(frame);
    }

    /// Milliseconds the last picture took to upload and convert (CPU side).
    pub fn upload_ms(&self) -> f32 {
        self.slot.upload_us.load(Ordering::Relaxed) as f32 / 1000.0
    }

    /// Draw the current picture into `rect`.
    pub fn paint(&self, painter: &egui::Painter, rect: egui::Rect) {
        painter.add(egui_wgpu::Callback::new_paint_callback(
            rect,
            VideoCallback {
                slot: self.slot.clone(),
            },
        ));
    }
}

/// Put the video pipelines into the renderer's callback resources. Call once per
/// render state (the window's, or a test harness's).
pub fn install(render_state: &egui_wgpu::RenderState) {
    let resources = Resources::new(&render_state.device, render_state.target_format);
    render_state
        .renderer
        .write()
        .callback_resources
        .insert(resources);
}

struct VideoCallback {
    slot: Arc<Slot>,
}

impl VideoCallback {
    fn key(&self) -> usize {
        Arc::as_ptr(&self.slot) as usize
    }
}

impl CallbackTrait for VideoCallback {
    fn prepare(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        _screen: &ScreenDescriptor,
        encoder: &mut wgpu::CommandEncoder,
        resources: &mut CallbackResources,
    ) -> Vec<wgpu::CommandBuffer> {
        let Some(res) = resources.get_mut::<Resources>() else {
            tracing::warn!("video pipelines not installed");
            return Vec::new();
        };
        // Tabs that are gone free their textures.
        res.tabs.retain(|_, t| t.slot.strong_count() > 0);
        let frame = self.slot.pending.lock().expect("video slot").take();
        let tab = res.tabs.entry(self.key()).or_insert_with(|| Tab {
            slot: Arc::downgrade(&self.slot),
            planes: None,
            frame: None,
        });
        if let Some(frame) = frame {
            let t0 = Instant::now();
            tab.update(&res.gpu, device, queue, encoder, &frame);
            let us = u32::try_from(t0.elapsed().as_micros()).unwrap_or(u32::MAX);
            self.slot.upload_us.store(us, Ordering::Relaxed);
        }
        Vec::new()
    }

    fn paint(
        &self,
        _info: egui::PaintCallbackInfo,
        pass: &mut wgpu::RenderPass<'static>,
        resources: &CallbackResources,
    ) {
        let Some(res) = resources.get::<Resources>() else {
            return;
        };
        let Some(frame) = res.tabs.get(&self.key()).and_then(|t| t.frame.as_ref()) else {
            return;
        };
        pass.set_pipeline(&res.gpu.display);
        pass.set_bind_group(0, &frame.display_bind, &[]);
        pass.draw(0..3, 0..1);
    }
}

/// Pipelines and samplers, shared by every tab.
struct Gpu {
    convert: wgpu::RenderPipeline,
    convert_layout: wgpu::BindGroupLayout,
    display: wgpu::RenderPipeline,
    display_layout: wgpu::BindGroupLayout,
    sampler: wgpu::Sampler,
    /// Stands in for I420's second chroma plane when the picture is NV12.
    dummy: wgpu::TextureView,
}

struct Resources {
    gpu: Gpu,
    tabs: HashMap<usize, Tab>,
}

impl Resources {
    fn new(device: &wgpu::Device, target: wgpu::TextureFormat) -> Self {
        Self {
            gpu: Gpu::new(device, target),
            tabs: HashMap::new(),
        }
    }
}

fn texture_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

fn fullscreen_pipeline(
    device: &wgpu::Device,
    label: &str,
    layout: &wgpu::BindGroupLayout,
    module: &wgpu::ShaderModule,
    fragment: &str,
    format: wgpu::TextureFormat,
) -> wgpu::RenderPipeline {
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(label),
        bind_group_layouts: &[Some(layout)],
        immediate_size: 0,
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(&pipeline_layout),
        vertex: wgpu::VertexState {
            module,
            entry_point: Some("vs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            buffers: &[],
        },
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module,
            entry_point: Some(fragment),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview_mask: None,
        cache: None,
    })
}

impl Gpu {
    fn new(device: &wgpu::Device, target: wgpu::TextureFormat) -> Self {
        let convert_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("rconn video convert"),
            source: wgpu::ShaderSource::Wgsl(format!("{FULLSCREEN_VS}{CONVERT_FS}").into()),
        });
        let display_module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("rconn video display"),
            source: wgpu::ShaderSource::Wgsl(format!("{FULLSCREEN_VS}{DISPLAY_FS}").into()),
        });
        let convert_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("rconn video planes"),
            entries: &[
                texture_entry(0),
                texture_entry(1),
                texture_entry(2),
                wgpu::BindGroupLayoutEntry {
                    binding: 3,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });
        let display_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("rconn video frame"),
            entries: &[
                texture_entry(0),
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });
        let convert = fullscreen_pipeline(
            device,
            "rconn video convert",
            &convert_layout,
            &convert_module,
            "fs_convert",
            FRAME_FORMAT,
        );
        let display = fullscreen_pipeline(
            device,
            "rconn video display",
            &display_layout,
            &display_module,
            if target.is_srgb() {
                "fs_linear"
            } else {
                "fs_gamma"
            },
            target,
        );
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("rconn video"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            ..Default::default()
        });
        let dummy = plane(
            device,
            "rconn video dummy",
            (1, 1),
            wgpu::TextureFormat::R8Unorm,
        )
        .create_view(&wgpu::TextureViewDescriptor::default());
        Self {
            convert,
            convert_layout,
            display,
            display_layout,
            sampler,
            dummy,
        }
    }
}

fn plane(
    device: &wgpu::Device,
    label: &str,
    (width, height): (usize, usize),
    format: wgpu::TextureFormat,
) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: extent((width, height)),
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

fn extent((width, height): (usize, usize)) -> wgpu::Extent3d {
    wgpu::Extent3d {
        width: width.max(1) as u32,
        height: height.max(1) as u32,
        depth_or_array_layers: 1,
    }
}

/// The YUV textures for one picture size and layout.
struct Planes {
    key: (usize, usize, PixelLayout),
    textures: Vec<(wgpu::Texture, usize)>,
    params: wgpu::Buffer,
    colour: Option<Colour>,
    bind: wgpu::BindGroup,
}

/// The persistent RGBA framebuffer of a tab.
struct Frame {
    size: (usize, usize),
    /// Kept for copies into and out of the picture (see the module docs).
    #[allow(dead_code)]
    texture: wgpu::Texture,
    view: wgpu::TextureView,
    display_bind: wgpu::BindGroup,
}

/// One tab's GPU state.
struct Tab {
    slot: Weak<Slot>,
    planes: Option<Planes>,
    frame: Option<Frame>,
}

impl Tab {
    /// Upload a picture and convert it into the frame texture.
    fn update(
        &mut self,
        gpu: &Gpu,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        f: &DecodedFrame,
    ) {
        let size = (f.width, f.height);
        if size.0 == 0 || size.1 == 0 {
            return;
        }
        let key = (f.width, f.height, f.layout);
        if self.planes.as_ref().is_none_or(|p| p.key != key) {
            self.planes = Some(Planes::new(gpu, device, key));
        }
        if self.frame.as_ref().is_none_or(|fr| fr.size != size) {
            self.frame = Some(Frame::new(gpu, device, size));
        }
        let (Some(planes), Some(frame)) = (self.planes.as_mut(), self.frame.as_ref()) else {
            return;
        };
        if planes.colour != Some(f.colour) {
            queue.write_buffer(&planes.params, 0, &params_bytes(f.colour, f.layout));
            planes.colour = Some(f.colour);
        }
        let sources: [&[u8]; 3] = [f.y(), f.chroma1(), f.chroma2()];
        for ((texture, bytes_per_px), data) in planes.textures.iter().zip(sources) {
            let size = texture.size();
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                data,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(size.width * *bytes_per_px as u32),
                    rows_per_image: Some(size.height),
                },
                size,
            );
        }
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("rconn video convert"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &frame.view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        pass.set_pipeline(&gpu.convert);
        pass.set_bind_group(0, &planes.bind, &[]);
        pass.draw(0..3, 0..1);
    }
}

/// The conversion's uniform block: the three matrix rows, then the layout flag.
fn params_bytes(colour: Colour, layout: PixelLayout) -> Vec<u8> {
    let rows = colour.yuv_to_rgb_rows();
    let i420 = if layout == PixelLayout::I420 {
        1.0
    } else {
        0.0
    };
    let mut v: Vec<f32> = rows.iter().flatten().copied().collect();
    v.extend([i420, 0.0, 0.0, 0.0]);
    bytemuck::cast_slice(&v).to_vec()
}

impl Planes {
    fn new(gpu: &Gpu, device: &wgpu::Device, key: (usize, usize, PixelLayout)) -> Self {
        let (w, h, layout) = key;
        let chroma = (w.div_ceil(2), h.div_ceil(2));
        use wgpu::TextureFormat::{R8Unorm, Rg8Unorm};
        let textures = match layout {
            PixelLayout::Nv12 => vec![
                (plane(device, "rconn video Y", (w, h), R8Unorm), 1),
                (plane(device, "rconn video CbCr", chroma, Rg8Unorm), 2),
            ],
            PixelLayout::I420 => vec![
                (plane(device, "rconn video Y", (w, h), R8Unorm), 1),
                (plane(device, "rconn video Cb", chroma, R8Unorm), 1),
                (plane(device, "rconn video Cr", chroma, R8Unorm), 1),
            ],
        };
        let views: Vec<wgpu::TextureView> = textures
            .iter()
            .map(|(t, _)| t.create_view(&wgpu::TextureViewDescriptor::default()))
            .collect();
        let params = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("rconn video colour"),
            size: 64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let view = |i: usize| views.get(i).unwrap_or(&gpu.dummy);
        let bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("rconn video planes"),
            layout: &gpu.convert_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(view(0)),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(view(1)),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::TextureView(view(2)),
                },
                wgpu::BindGroupEntry {
                    binding: 3,
                    resource: params.as_entire_binding(),
                },
            ],
        });
        Self {
            key,
            textures,
            params,
            colour: None,
            bind,
        }
    }
}

impl Frame {
    fn new(gpu: &Gpu, device: &wgpu::Device, size: (usize, usize)) -> Self {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("rconn video frame"),
            size: extent(size),
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FRAME_FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let display_bind = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("rconn video frame"),
            layout: &gpu.display_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::Sampler(&gpu.sampler),
                },
            ],
        });
        Self {
            size,
            texture,
            view,
            display_bind,
        }
    }
}

#[cfg(test)]
mod tests {
    //! GPU conversion against the CPU reference, headless (`cargo test -p rconn-viewer
    //! video_render -- --nocapture` also prints upload timings).
    use super::*;
    use rconn_media::sps::Matrix;

    /// A headless device: the test harness's (a software rasteriser where there is
    /// one, for repeatable results), or the one the viewer window would get (`gpu`).
    fn render_state(gpu: bool) -> egui_wgpu::RenderState {
        let setup = if gpu {
            egui_wgpu::WgpuSetup::CreateNew(egui_wgpu::WgpuSetupCreateNew::without_display_handle())
        } else {
            egui_kittest::wgpu::default_wgpu_setup()
        };
        egui_kittest::wgpu::create_render_state(setup, egui_wgpu::RendererOptions::PREDICTABLE)
    }

    /// A picture with every kind of content: gradients, saturated colours, odd edges.
    fn picture(w: usize, h: usize, layout: PixelLayout, colour: Colour) -> DecodedFrame {
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let mut data = Vec::with_capacity(w * h + 2 * cw * ch);
        for i in 0..w * h {
            let (x, y) = (i % w, i / w);
            data.push(((x * 7 + y * 3) % 256) as u8);
        }
        let u = |x: usize, y: usize| ((x * 13 + y * 5 + 40) % 256) as u8;
        let v = |x: usize, y: usize| ((x * 3 + y * 11 + 200) % 256) as u8;
        match layout {
            PixelLayout::Nv12 => {
                for i in 0..cw * ch {
                    data.extend([u(i % cw, i / cw), v(i % cw, i / cw)]);
                }
            }
            PixelLayout::I420 => {
                data.extend((0..cw * ch).map(|i| u(i % cw, i / cw)));
                data.extend((0..cw * ch).map(|i| v(i % cw, i / cw)));
            }
        }
        DecodedFrame {
            width: w,
            height: h,
            layout,
            data,
            colour,
            decode_time: std::time::Duration::ZERO,
        }
    }

    /// One tab's GPU state on a headless device.
    struct Rig {
        rs: egui_wgpu::RenderState,
        gpu: Gpu,
        _slot: Arc<Slot>,
        tab: Tab,
    }

    impl Rig {
        fn new(gpu: bool) -> Self {
            let rs = render_state(gpu);
            let gpu = Gpu::new(&rs.device, rs.target_format);
            let slot = Arc::new(Slot::default());
            let tab = Tab {
                slot: Arc::downgrade(&slot),
                planes: None,
                frame: None,
            };
            Self {
                rs,
                gpu,
                _slot: slot,
                tab,
            }
        }

        /// Upload and convert (CPU ms for that), without reading back.
        fn upload(&mut self, f: &DecodedFrame) -> (wgpu::CommandEncoder, f64) {
            let (device, queue) = (&self.rs.device, &self.rs.queue);
            let mut encoder = device.create_command_encoder(&Default::default());
            let t0 = Instant::now();
            self.tab.update(&self.gpu, device, queue, &mut encoder, f);
            (encoder, t0.elapsed().as_secs_f64() * 1e3)
        }

        /// Convert on the GPU and read the frame texture back as RGBA.
        fn convert(&mut self, f: &DecodedFrame) -> Vec<u8> {
            let (mut encoder, _) = self.upload(f);
            let (device, queue) = (&self.rs.device, &self.rs.queue);
            let (w, h) = (f.width as u32, f.height as u32);
            let row = (w * 4).next_multiple_of(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT);
            let readback = device.create_buffer(&wgpu::BufferDescriptor {
                label: None,
                size: u64::from(row * h),
                usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                mapped_at_creation: false,
            });
            let frame = self.tab.frame.as_ref().expect("frame texture");
            encoder.copy_texture_to_buffer(
                frame.texture.as_image_copy(),
                wgpu::TexelCopyBufferInfo {
                    buffer: &readback,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: 0,
                        bytes_per_row: Some(row),
                        rows_per_image: Some(h),
                    },
                },
                frame.texture.size(),
            );
            queue.submit([encoder.finish()]);
            readback
                .slice(..)
                .map_async(wgpu::MapMode::Read, |r| r.expect("map"));
            device
                .poll(wgpu::PollType::Wait {
                    submission_index: None,
                    timeout: None,
                })
                .expect("poll");
            let mapped = readback.slice(..).get_mapped_range().expect("mapped");
            let mut out = Vec::with_capacity((w * h * 4) as usize);
            for r in mapped.chunks_exact(row as usize) {
                out.extend_from_slice(&r[..(w * 4) as usize]);
            }
            out
        }
    }

    fn worst(a: &[u8], b: &[u8]) -> u8 {
        assert_eq!(a.len(), b.len());
        a.iter()
            .zip(b)
            .map(|(x, y)| x.abs_diff(*y))
            .max()
            .unwrap_or(0)
    }

    #[test]
    fn gpu_conversion_matches_the_cpu_reference() {
        let cases = [
            (PixelLayout::Nv12, Colour::default()),
            (PixelLayout::I420, Colour::default()),
            (
                PixelLayout::Nv12,
                Colour {
                    matrix: Matrix::Bt709,
                    full_range: false,
                },
            ),
            (
                PixelLayout::I420,
                Colour {
                    matrix: Matrix::Bt709,
                    full_range: true,
                },
            ),
            (
                PixelLayout::Nv12,
                Colour {
                    matrix: Matrix::Bt2020,
                    full_range: false,
                },
            ),
        ];
        // The harness's device and the one a window gets (they differ on Windows VMs:
        // a software rasteriser, and GL or D3D12 on the GPU).
        for gpu in [false, true] {
            let mut rig = Rig::new(gpu);
            let adapter = rig.rs.adapter.get_info();
            for (layout, colour) in cases {
                // Odd-ish sizes: chroma rounding and rows not a multiple of 256 bytes.
                let f = picture(322, 182, layout, colour);
                let out = rig.convert(&f);
                let cpu = f.to_rgba();
                let d = worst(&out, &cpu);
                // BT.601 limited: the CPU uses openh264's integer formula (within 2 of
                // exact); others use the same float maths as the shader.
                let limit = if colour == Colour::default() { 2 } else { 1 };
                assert!(
                    d <= limit,
                    "{} ({:?}): {layout:?} {colour:?}: off by {d}",
                    adapter.name,
                    adapter.backend
                );
            }
        }
    }

    #[test]
    fn known_colours_come_out_right() {
        let mut rig = Rig::new(false);
        // Limited-range BT.601: white, black, mid grey.
        for (y, rgb) in [(235u8, 255u8), (16, 0), (126, 128)] {
            let mut f = picture(4, 2, PixelLayout::Nv12, Colour::default());
            f.data = [vec![y; 8], vec![128; 4]].concat();
            let gpu = rig.convert(&f);
            for px in gpu.as_chunks::<4>().0 {
                assert!(
                    px[..3].iter().all(|&c| c.abs_diff(rgb) <= 1),
                    "Y {y}: {px:?}"
                );
                assert_eq!(px[3], 255);
            }
        }
    }

    /// Prints what a picture costs to put on the GPU, old way (RGBA through egui's
    /// texture upload) and new (planes + shader). `--ignored --nocapture`.
    #[test]
    #[ignore = "timing, run by hand"]
    fn upload_speed() {
        let mut rig = Rig::new(true);
        println!("adapter: {:?}", rig.rs.adapter.get_info());
        let wait = |rs: &egui_wgpu::RenderState| {
            rs.device
                .poll(wgpu::PollType::Wait {
                    submission_index: None,
                    timeout: None,
                })
                .expect("poll");
        };
        for (w, h) in [(1920, 1080), (2992, 1878)] {
            let f = picture(w, h, PixelLayout::Nv12, Colour::default());
            let n = 30;
            let (mut new_ms, mut new_total) = (0.0, 0.0);
            rig.convert(&f); // textures created
            for _ in 0..n {
                let t0 = Instant::now();
                let (encoder, ms) = rig.upload(&f);
                rig.rs.queue.submit([encoder.finish()]);
                wait(&rig.rs);
                new_total += t0.elapsed().as_secs_f64() * 1e3;
                new_ms += ms;
            }
            let rs = &rig.rs;
            // Old: RGBA on the CPU (as the decoder used to), a copy into an egui image,
            // and egui's texture upload.
            let rgba = f.to_rgba();
            let mut renderer = rs.renderer.write();
            let id = egui::TextureId::User(999);
            let (mut old_ms, mut old_total) = (0.0, 0.0);
            for _ in 0..n {
                let t0 = Instant::now();
                let img = egui::ColorImage::new(
                    [w, h],
                    bytemuck::cast_slice::<u8, egui::Color32>(&rgba).to_vec(),
                );
                let delta = egui::epaint::ImageDelta::full(
                    egui::ImageData::Color(Arc::new(img)),
                    egui::TextureOptions::LINEAR,
                );
                renderer.update_texture(&rs.device, &rs.queue, id, &delta);
                old_ms += t0.elapsed().as_secs_f64() * 1e3;
                rs.queue.submit([]);
                wait(rs);
                old_total += t0.elapsed().as_secs_f64() * 1e3;
            }
            let t0 = Instant::now();
            for _ in 0..5 {
                std::hint::black_box(f.to_rgba());
            }
            let rgba_ms = t0.elapsed().as_secs_f64() * 1e3 / 5.0;
            let n = f64::from(n);
            println!(
                "{w}x{h}: old: CPU YUV->RGBA {rgba_ms:.2} ms + RGBA upload {:.2} ms ({:.2} ms until the GPU is done); \
                 new: NV12 upload + shader {:.2} ms ({:.2} ms until done)",
                old_ms / n,
                old_total / n,
                new_ms / n,
                new_total / n
            );
        }
    }
}
