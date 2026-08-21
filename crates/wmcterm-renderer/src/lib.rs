//! GPU presentation boundary for terminal snapshots.

use std::sync::Arc;

use anyhow::{Context, Result};
use bytemuck::{Pod, Zeroable};
use glyphon::cosmic_text::{Fallback, PlatformFallback};
use glyphon::{
    Attrs, Buffer, Cache, Color, Family, FontSystem, Metrics, Resolution, Shaping, Style,
    SwashCache, TextArea, TextAtlas, TextBounds, TextRenderer, Viewport, Weight,
};
use unicode_script::Script;
use wgpu::{
    CommandEncoderDescriptor, CompositeAlphaMode, DeviceDescriptor, Instance, InstanceDescriptor,
    LoadOp, MultisampleState, Operations, PresentMode, RenderPassColorAttachment,
    RenderPassDescriptor, RequestAdapterOptions, SurfaceColorSpace, SurfaceConfiguration,
    TextureFormat, TextureUsages, TextureViewDescriptor,
};
use winit::{event_loop::ActiveEventLoop, window::Window};
pub use wmcterm_terminal::ScreenSnapshot;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CellMetrics {
    pub width: f32,
    pub height: f32,
    pub baseline: f32,
    pub underline_y: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TextBlock {
    pub text: String,
    pub spans: Vec<TextSpan>,
    pub left: f32,
    pub top: f32,
    pub width: f32,
    pub height: f32,
    pub color: [u8; 4],
    pub font_size: f32,
    pub line_height: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TextSpan {
    pub text: String,
    pub color: [u8; 4],
    pub bold: bool,
    pub italic: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RectBlock {
    pub left: f32,
    pub top: f32,
    pub width: f32,
    pub height: f32,
    pub color: [u8; 4],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
struct RectVertex {
    position: [f32; 2],
    color: [f32; 4],
}

struct PreparedBlock {
    buffer: Buffer,
    left: f32,
    top: f32,
    bounds: TextBounds,
    color: Color,
}

pub fn preferred_backends() -> wgpu::Backends {
    wgpu::Backends::from_env().unwrap_or(wgpu::Backends::PRIMARY)
}

pub fn default_window_attributes() -> winit::window::WindowAttributes {
    Window::default_attributes().with_title("wmcterm")
}

const MAPLE_MONO_FAMILY: &str = "Maple Mono Normal NF";
const MAPLE_MONO_REGULAR: &[u8] = include_bytes!(
    "../../../assets/fonts/MapleMonoNormal-NF-unhinted/MapleMonoNormal-NF-Regular.ttf"
);
const MAPLE_MONO_BOLD: &[u8] =
    include_bytes!("../../../assets/fonts/MapleMonoNormal-NF-unhinted/MapleMonoNormal-NF-Bold.ttf");
const MAPLE_MONO_ITALIC: &[u8] = include_bytes!(
    "../../../assets/fonts/MapleMonoNormal-NF-unhinted/MapleMonoNormal-NF-Italic.ttf"
);
const MAPLE_MONO_BOLD_ITALIC: &[u8] = include_bytes!(
    "../../../assets/fonts/MapleMonoNormal-NF-unhinted/MapleMonoNormal-NF-BoldItalic.ttf"
);
const TERMINAL_CELL_WIDTH_FACTOR: f32 = 0.6;

// Prefer fonts designed for terminal cells. Platform UI fonts remain last-resort
// fallbacks; the renderer still snaps their advance to the terminal grid.
const CJK_SC_TERMINAL_FALLBACKS: &[&str] = &[
    "Maple Mono Normal NF CN",
    "Sarasa Mono SC",
    "Sarasa Fixed SC",
    "Noto Sans Mono CJK SC",
    "WenQuanYi Zen Hei Mono",
    "WenQuanYi Micro Hei Mono",
    "NSimSun",
    "SimSun",
    "Microsoft YaHei UI",
    "Microsoft YaHei",
    "Noto Sans CJK SC",
    "WenQuanYi Zen Hei",
];
const CJK_TC_TERMINAL_FALLBACKS: &[&str] = &[
    "Sarasa Mono TC",
    "Sarasa Fixed TC",
    "Noto Sans Mono CJK TC",
    "MingLiU",
    "Microsoft JhengHei UI",
    "Microsoft JhengHei",
    "Noto Sans CJK TC",
];
const CJK_HK_TERMINAL_FALLBACKS: &[&str] = &[
    "Sarasa Mono HC",
    "Sarasa Fixed HC",
    "Noto Sans Mono CJK HK",
    "MingLiU_HKSCS",
    "Microsoft JhengHei UI",
    "Noto Sans CJK HK",
];
const CJK_JP_TERMINAL_FALLBACKS: &[&str] = &[
    "Sarasa Mono J",
    "Sarasa Fixed J",
    "Noto Sans Mono CJK JP",
    "BIZ UDGothic",
    "MS Gothic",
    "Yu Gothic UI",
    "Noto Sans CJK JP",
];
const CJK_KR_TERMINAL_FALLBACKS: &[&str] = &[
    "D2Coding",
    "Sarasa Mono K",
    "Sarasa Fixed K",
    "Noto Sans Mono CJK KR",
    "GulimChe",
    "BatangChe",
    "Malgun Gothic",
    "Noto Sans CJK KR",
];

#[derive(Debug)]
struct TerminalFontFallback(PlatformFallback);

impl Fallback for TerminalFontFallback {
    fn common_fallback(&self) -> &[&'static str] {
        self.0.common_fallback()
    }

    fn forbidden_fallback(&self) -> &[&'static str] {
        self.0.forbidden_fallback()
    }

    fn script_fallback(&self, script: Script, locale: &str) -> &[&'static str] {
        match script {
            Script::Han | Script::Bopomofo => cjk_terminal_fallbacks(locale),
            Script::Hiragana | Script::Katakana => CJK_JP_TERMINAL_FALLBACKS,
            Script::Hangul => CJK_KR_TERMINAL_FALLBACKS,
            _ => self.0.script_fallback(script, locale),
        }
    }
}

fn cjk_terminal_fallbacks(locale: &str) -> &'static [&'static str] {
    let locale = locale.to_ascii_lowercase().replace('_', "-");
    if locale.starts_with("zh-tw") || locale.starts_with("zh-hant") {
        CJK_TC_TERMINAL_FALLBACKS
    } else if locale.starts_with("zh-hk") || locale.starts_with("zh-mo") {
        CJK_HK_TERMINAL_FALLBACKS
    } else if locale.starts_with("ja") {
        CJK_JP_TERMINAL_FALLBACKS
    } else if locale.starts_with("ko") {
        CJK_KR_TERMINAL_FALLBACKS
    } else {
        CJK_SC_TERMINAL_FALLBACKS
    }
}

pub struct GpuRenderer {
    instance: Instance,
    device: wgpu::Device,
    queue: wgpu::Queue,
    surface: wgpu::Surface<'static>,
    config: SurfaceConfiguration,
    font_system: FontSystem,
    swash_cache: SwashCache,
    viewport: Viewport,
    atlas: TextAtlas,
    text_renderer: TextRenderer,
    overlay_text_renderer: TextRenderer,
    font_family: String,
    blocks: Vec<PreparedBlock>,
    overlay_blocks: Vec<PreparedBlock>,
    scene_blocks: Option<Vec<TextBlock>>,
    scene_rectangles: Option<Vec<RectBlock>>,
    scene_overlay_blocks: Option<Vec<TextBlock>>,
    scene_overlay_rectangles: Option<Vec<RectBlock>>,
    rect_pipeline: wgpu::RenderPipeline,
    rect_buffer: wgpu::Buffer,
    rect_vertex_count: u32,
    overlay_rect_buffer: wgpu::Buffer,
    overlay_rect_vertex_count: u32,
    rect_buffer_capacity: usize,
    overlay_rect_buffer_capacity: usize,
    supported_alpha_modes: Vec<CompositeAlphaMode>,
    transparent: bool,
    scale_factor: f64,
    window: Arc<Window>,
}

impl GpuRenderer {
    pub fn new(
        window: Arc<Window>,
        event_loop: &ActiveEventLoop,
        font_family: impl Into<String>,
        transparent: bool,
        scale_factor: f64,
    ) -> Result<Self> {
        pollster::block_on(Self::new_async(
            window,
            event_loop,
            font_family.into(),
            transparent,
            scale_factor,
        ))
    }

    async fn new_async(
        window: Arc<Window>,
        event_loop: &ActiveEventLoop,
        font_family: String,
        transparent: bool,
        scale_factor: f64,
    ) -> Result<Self> {
        let size = window.inner_size();
        window.set_transparent(transparent);
        let instance = Instance::new(InstanceDescriptor::new_with_display_handle_from_env(
            Box::new(event_loop.owned_display_handle()),
        ));
        let adapter = instance
            .request_adapter(&RequestAdapterOptions::default())
            .await
            .context("no compatible GPU adapter")?;
        let (device, queue) = adapter
            .request_device(&DeviceDescriptor::default())
            .await
            .context("requesting GPU device")?;
        let surface = instance
            .create_surface(window.clone())
            .context("creating window surface")?;
        let surface_capabilities = surface.get_capabilities(&adapter);
        let supported_alpha_modes = surface_capabilities.alpha_modes;
        let format = TextureFormat::Bgra8UnormSrgb;
        let alpha_mode = select_alpha_mode(transparent, &supported_alpha_modes);
        let config = SurfaceConfiguration {
            usage: TextureUsages::RENDER_ATTACHMENT,
            format,
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: PresentMode::Fifo,
            alpha_mode,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
            color_space: SurfaceColorSpace::Auto,
        };
        surface.configure(&device, &config);

        let mut font_db = glyphon::cosmic_text::fontdb::Database::new();
        font_db.load_system_fonts();
        for font in [
            MAPLE_MONO_REGULAR,
            MAPLE_MONO_BOLD,
            MAPLE_MONO_ITALIC,
            MAPLE_MONO_BOLD_ITALIC,
        ] {
            font_db.load_font_data(font.to_vec());
        }
        font_db.set_monospace_family(MAPLE_MONO_FAMILY);
        let locale = sys_locale::get_locale().unwrap_or_else(|| "en-US".to_owned());
        let font_system = FontSystem::new_with_locale_and_db_and_fallback(
            locale,
            font_db,
            TerminalFontFallback(PlatformFallback),
        );
        let swash_cache = SwashCache::new();
        let cache = Cache::new(&device);
        let viewport = Viewport::new(&device, &cache);
        let mut atlas = TextAtlas::new(&device, &queue, &cache, format);
        let text_renderer =
            TextRenderer::new(&mut atlas, &device, MultisampleState::default(), None);
        let overlay_text_renderer =
            TextRenderer::new(&mut atlas, &device, MultisampleState::default(), None);
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("wmcterm rect shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("rect.wgsl").into()),
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("wmcterm rect layout"),
            bind_group_layouts: &[],
            immediate_size: 0,
        });
        let rect_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("wmcterm rect pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[Some(wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<RectVertex>() as u64,
                    step_mode: wgpu::VertexStepMode::Vertex,
                    attributes: &wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x4],
                })],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let rect_buffer = create_rect_buffer(&device, "wmcterm scene rects", 1);
        let overlay_rect_buffer = create_rect_buffer(&device, "wmcterm overlay rects", 1);
        Ok(Self {
            instance,
            device,
            queue,
            surface,
            config,
            font_system,
            swash_cache,
            viewport,
            atlas,
            text_renderer,
            overlay_text_renderer,
            font_family,
            blocks: Vec::new(),
            overlay_blocks: Vec::new(),
            scene_blocks: None,
            scene_rectangles: None,
            scene_overlay_blocks: None,
            scene_overlay_rectangles: None,
            rect_pipeline,
            rect_buffer,
            rect_vertex_count: 0,
            overlay_rect_buffer,
            overlay_rect_vertex_count: 0,
            rect_buffer_capacity: 1,
            overlay_rect_buffer_capacity: 1,
            supported_alpha_modes,
            transparent: transparent && alpha_mode != CompositeAlphaMode::Opaque,
            scale_factor: normalize_scale_factor(scale_factor),
            window,
        })
    }

    pub fn window(&self) -> &Arc<Window> {
        &self.window
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
        self.scene_rectangles = None;
        self.scene_overlay_rectangles = None;
    }

    pub fn set_transparent(&mut self, transparent: bool) {
        self.window.set_transparent(transparent);
        let alpha_mode = select_alpha_mode(transparent, &self.supported_alpha_modes);
        self.transparent = transparent && alpha_mode != CompositeAlphaMode::Opaque;
        if self.config.alpha_mode != alpha_mode {
            self.config.alpha_mode = alpha_mode;
            self.surface.configure(&self.device, &self.config);
        }
    }

    pub fn transparency_active(&self) -> bool {
        self.transparent
    }

    pub fn set_scale_factor(&mut self, scale_factor: f64) {
        let scale_factor = normalize_scale_factor(scale_factor);
        if self.scale_factor != scale_factor {
            self.scale_factor = scale_factor;
            // Keep the prepared content until the application submits its next scene, but
            // invalidate equality caches so that identical logical content is re-scaled.
            self.scene_blocks = None;
            self.scene_rectangles = None;
            self.scene_overlay_blocks = None;
            self.scene_overlay_rectangles = None;
        }
    }

    pub fn set_snapshot(&mut self, snapshot: &ScreenSnapshot) {
        self.set_scene(vec![TextBlock {
            text: snapshot.lines.join("\n"),
            spans: snapshot
                .cells
                .iter()
                .enumerate()
                .flat_map(|(row_index, row)| {
                    row.iter()
                        .map(|cell| TextSpan {
                            text: cell.text.clone(),
                            color: [cell.foreground.r, cell.foreground.g, cell.foreground.b, 255],
                            bold: cell.bold,
                            italic: cell.italic,
                        })
                        .chain((row_index + 1 < snapshot.cells.len()).then_some(TextSpan {
                            text: "\n".into(),
                            color: [220, 223, 228, 255],
                            bold: false,
                            italic: false,
                        }))
                })
                .collect(),
            left: 12.0,
            top: 10.0,
            width: self.config.width as f32 / self.scale_factor as f32 - 12.0,
            height: self.config.height as f32 / self.scale_factor as f32 - 10.0,
            color: [220, 223, 228, 255],
            font_size: 16.0,
            line_height: 21.0,
        }]);
    }

    pub fn set_scene(&mut self, blocks: Vec<TextBlock>) {
        self.set_scene_with_rects(blocks, Vec::new());
    }

    pub fn set_scene_with_rects(&mut self, blocks: Vec<TextBlock>, rectangles: Vec<RectBlock>) {
        self.overlay_blocks.clear();
        self.overlay_rect_vertex_count = 0;
        self.scene_overlay_blocks = None;
        self.scene_overlay_rectangles = None;
        self.update_scene_blocks(blocks, rectangles);
    }

    pub fn set_scene_with_overlays(
        &mut self,
        blocks: Vec<TextBlock>,
        rectangles: Vec<RectBlock>,
        overlay_blocks: Vec<TextBlock>,
        overlay_rectangles: Vec<RectBlock>,
    ) {
        self.update_scene_blocks(blocks, rectangles);
        update_prepared_text_blocks(
            &mut self.font_system,
            &self.font_family,
            self.scale_factor,
            &mut self.scene_overlay_blocks,
            &mut self.overlay_blocks,
            overlay_blocks,
        );
        if self.scene_overlay_rectangles.as_ref() != Some(&overlay_rectangles) {
            let vertices = rect_vertices(
                &overlay_rectangles,
                self.config.width,
                self.config.height,
                self.scale_factor,
            );
            self.overlay_rect_vertex_count = vertices.len() as u32;
            update_rect_buffer(
                &self.device,
                &self.queue,
                &mut self.overlay_rect_buffer,
                &mut self.overlay_rect_buffer_capacity,
                "wmcterm overlay rects",
                &vertices,
            );
            self.scene_overlay_rectangles = Some(overlay_rectangles);
        }
    }

    fn update_scene_blocks(&mut self, blocks: Vec<TextBlock>, rectangles: Vec<RectBlock>) {
        update_prepared_text_blocks(
            &mut self.font_system,
            &self.font_family,
            self.scale_factor,
            &mut self.scene_blocks,
            &mut self.blocks,
            blocks,
        );
        if self.scene_rectangles.as_ref() != Some(&rectangles) {
            let vertices = rect_vertices(
                &rectangles,
                self.config.width,
                self.config.height,
                self.scale_factor,
            );
            self.rect_vertex_count = vertices.len() as u32;
            update_rect_buffer(
                &self.device,
                &self.queue,
                &mut self.rect_buffer,
                &mut self.rect_buffer_capacity,
                "wmcterm scene rects",
                &vertices,
            );
            self.scene_rectangles = Some(rectangles);
        }
    }

    pub fn render(&mut self) -> Result<()> {
        self.viewport.update(
            &self.queue,
            Resolution {
                width: self.config.width,
                height: self.config.height,
            },
        );
        self.text_renderer.prepare(
            &self.device,
            &self.queue,
            &mut self.font_system,
            &mut self.atlas,
            &self.viewport,
            self.blocks.iter().map(|block| TextArea {
                buffer: &block.buffer,
                left: block.left,
                top: block.top,
                scale: 1.0,
                bounds: block.bounds,
                default_color: block.color,
                custom_glyphs: &[],
            }),
            &mut self.swash_cache,
        )?;
        self.overlay_text_renderer.prepare(
            &self.device,
            &self.queue,
            &mut self.font_system,
            &mut self.atlas,
            &self.viewport,
            self.overlay_blocks.iter().map(|block| TextArea {
                buffer: &block.buffer,
                left: block.left,
                top: block.top,
                scale: 1.0,
                bounds: block.bounds,
                default_color: block.color,
                custom_glyphs: &[],
            }),
            &mut self.swash_cache,
        )?;

        let frame = match self.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame) => frame,
            wgpu::CurrentSurfaceTexture::Timeout | wgpu::CurrentSurfaceTexture::Occluded => {
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Outdated => {
                self.surface.configure(&self.device, &self.config);
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Suboptimal(frame) => {
                drop(frame);
                self.surface.configure(&self.device, &self.config);
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Lost => {
                self.surface = self.instance.create_surface(self.window.clone())?;
                self.surface.configure(&self.device, &self.config);
                return Ok(());
            }
            wgpu::CurrentSurfaceTexture::Validation => anyhow::bail!("wgpu validation error"),
        };
        let view = frame.texture.create_view(&TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&CommandEncoderDescriptor {
                label: Some("wmcterm"),
            });
        {
            let mut pass = encoder.begin_render_pass(&RenderPassDescriptor {
                label: Some("wmcterm terminal pass"),
                color_attachments: &[Some(RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: Operations {
                        load: LoadOp::Clear(wgpu::Color {
                            r: srgb_byte_to_linear(28) as f64,
                            g: srgb_byte_to_linear(28) as f64,
                            b: srgb_byte_to_linear(28) as f64,
                            a: if self.transparent { 0.0 } else { 1.0 },
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            if self.rect_vertex_count > 0 {
                pass.set_pipeline(&self.rect_pipeline);
                pass.set_vertex_buffer(0, self.rect_buffer.slice(..));
                pass.draw(0..self.rect_vertex_count, 0..1);
            }
            self.text_renderer
                .render(&self.atlas, &self.viewport, &mut pass)?;
            if self.overlay_rect_vertex_count > 0 {
                pass.set_pipeline(&self.rect_pipeline);
                pass.set_vertex_buffer(0, self.overlay_rect_buffer.slice(..));
                pass.draw(0..self.overlay_rect_vertex_count, 0..1);
            }
            self.overlay_text_renderer
                .render(&self.atlas, &self.viewport, &mut pass)?;
        }
        self.queue.submit(Some(encoder.finish()));
        self.queue.present(frame);
        self.atlas.trim();
        Ok(())
    }
}

fn prepare_text_blocks(
    font_system: &mut FontSystem,
    font_family: &str,
    blocks: &[TextBlock],
    scale_factor: f64,
) -> Vec<PreparedBlock> {
    let scale_factor = normalize_scale_factor(scale_factor) as f32;
    blocks
        .iter()
        .map(|block| {
            let left = block.left * scale_factor;
            let top = block.top * scale_factor;
            let width = block.width * scale_factor;
            let height = block.height * scale_factor;
            let font_size = block.font_size * scale_factor;
            let line_height = block.line_height * scale_factor;
            let mut buffer = Buffer::new(&mut *font_system, Metrics::new(font_size, line_height));
            buffer.set_size(Some(width.max(1.0)), Some(height.max(1.0)));
            buffer.set_monospace_width(Some(font_size * TERMINAL_CELL_WIDTH_FACTOR));
            let family = if font_family.trim().is_empty() {
                MAPLE_MONO_FAMILY
            } else {
                font_family
            };
            let default_attrs = Attrs::new().family(Family::Name(family));
            if block.spans.is_empty() {
                buffer.set_text(&block.text, &default_attrs, Shaping::Advanced, None);
            } else {
                buffer.set_rich_text(
                    block.spans.iter().map(|span| {
                        let attrs = default_attrs
                            .clone()
                            .color(Color::rgba(
                                span.color[0],
                                span.color[1],
                                span.color[2],
                                span.color[3],
                            ))
                            .weight(if span.bold {
                                Weight::BOLD
                            } else {
                                Weight::NORMAL
                            })
                            .style(if span.italic {
                                Style::Italic
                            } else {
                                Style::Normal
                            });
                        (span.text.as_str(), attrs)
                    }),
                    &default_attrs,
                    Shaping::Advanced,
                    None,
                );
            }
            buffer.shape_until_scroll(&mut *font_system, false);
            PreparedBlock {
                buffer,
                left,
                top,
                bounds: TextBounds {
                    left: left as i32,
                    top: top as i32,
                    right: (left + width) as i32,
                    bottom: (top + height) as i32,
                },
                color: Color::rgba(
                    block.color[0],
                    block.color[1],
                    block.color[2],
                    block.color[3],
                ),
            }
        })
        .collect()
}

fn update_prepared_text_blocks(
    font_system: &mut FontSystem,
    font_family: &str,
    scale_factor: f64,
    previous_blocks: &mut Option<Vec<TextBlock>>,
    prepared_blocks: &mut Vec<PreparedBlock>,
    next_blocks: Vec<TextBlock>,
) {
    if previous_blocks.as_ref() == Some(&next_blocks) {
        return;
    }

    let mut previous_specs = previous_blocks.take().unwrap_or_default().into_iter();
    let mut previous_prepared = std::mem::take(prepared_blocks).into_iter();
    let mut next_prepared = Vec::with_capacity(next_blocks.len());
    for next in &next_blocks {
        match (previous_specs.next(), previous_prepared.next()) {
            (Some(previous), Some(prepared)) if previous == *next => {
                next_prepared.push(prepared);
            }
            _ => {
                next_prepared.extend(prepare_text_blocks(
                    font_system,
                    font_family,
                    std::slice::from_ref(next),
                    scale_factor,
                ));
            }
        }
    }
    *prepared_blocks = next_prepared;
    *previous_blocks = Some(next_blocks);
}

fn rect_vertices(
    rectangles: &[RectBlock],
    width: u32,
    height: u32,
    scale_factor: f64,
) -> Vec<RectVertex> {
    let scale_factor = normalize_scale_factor(scale_factor) as f32;
    let mut vertices = Vec::with_capacity(rectangles.len() * 6);
    for rect in rectangles {
        let left = rect.left * scale_factor / width as f32 * 2.0 - 1.0;
        let right = (rect.left + rect.width) * scale_factor / width as f32 * 2.0 - 1.0;
        let top = 1.0 - rect.top * scale_factor / height as f32 * 2.0;
        let bottom = 1.0 - (rect.top + rect.height) * scale_factor / height as f32 * 2.0;
        let color = rgba_to_gpu(rect.color);
        vertices.extend(
            [
                [left, top],
                [left, bottom],
                [right, bottom],
                [left, top],
                [right, bottom],
                [right, top],
            ]
            .map(|position| RectVertex { position, color }),
        );
    }
    vertices
}

fn normalize_scale_factor(scale_factor: f64) -> f64 {
    if scale_factor.is_finite() && scale_factor > 0.0 {
        scale_factor
    } else {
        1.0
    }
}

fn select_alpha_mode(
    transparent: bool,
    supported_alpha_modes: &[CompositeAlphaMode],
) -> CompositeAlphaMode {
    if !transparent {
        return CompositeAlphaMode::Opaque;
    }
    [
        CompositeAlphaMode::PreMultiplied,
        CompositeAlphaMode::PostMultiplied,
        CompositeAlphaMode::Inherit,
    ]
    .into_iter()
    .find(|mode| supported_alpha_modes.contains(mode))
    .or_else(|| {
        supported_alpha_modes
            .contains(&CompositeAlphaMode::Opaque)
            .then_some(CompositeAlphaMode::Opaque)
    })
    .or_else(|| supported_alpha_modes.first().copied())
    .unwrap_or(CompositeAlphaMode::Opaque)
}

fn create_rect_buffer(
    device: &wgpu::Device,
    label: &'static str,
    vertex_capacity: usize,
) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size: (vertex_capacity.max(1) * std::mem::size_of::<RectVertex>()) as u64,
        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

fn update_rect_buffer(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    buffer: &mut wgpu::Buffer,
    capacity: &mut usize,
    label: &'static str,
    vertices: &[RectVertex],
) {
    if vertices.is_empty() {
        return;
    }
    if vertices.len() > *capacity {
        *capacity = vertices.len().next_power_of_two();
        *buffer = create_rect_buffer(device, label, *capacity);
    }
    queue.write_buffer(buffer, 0, bytemuck::cast_slice(vertices));
}

fn srgb_byte_to_linear(value: u8) -> f32 {
    let channel = value as f32 / 255.0;
    if channel <= 0.040_45 {
        channel / 12.92
    } else {
        ((channel + 0.055) / 1.055).powf(2.4)
    }
}

fn rgba_to_gpu([red, green, blue, alpha]: [u8; 4]) -> [f32; 4] {
    [
        srgb_byte_to_linear(red),
        srgb_byte_to_linear(green),
        srgb_byte_to_linear(blue),
        alpha as f32 / 255.0,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renderer_defaults_have_a_window_title_and_gpu_backend() {
        let attributes = default_window_attributes();
        assert_eq!(attributes.title, "wmcterm");
        assert!(!preferred_backends().is_empty());
    }

    #[test]
    fn converts_configured_srgb_colors_for_the_gpu_target() {
        assert!((srgb_byte_to_linear(0) - 0.0).abs() < f32::EPSILON);
        assert!((srgb_byte_to_linear(255) - 1.0).abs() < f32::EPSILON);
        assert!((srgb_byte_to_linear(30) - 0.012_983).abs() < 0.000_01);
        let converted = rgba_to_gpu([30, 30, 46, 128]);
        assert!((converted[3] - 128.0 / 255.0).abs() < f32::EPSILON);
    }

    #[test]
    fn chinese_fallbacks_prefer_terminal_monospace_fonts() {
        let simplified = cjk_terminal_fallbacks("zh_CN.UTF-8");
        assert_eq!(simplified[0], "Maple Mono Normal NF CN");
        assert_eq!(simplified[1], "Sarasa Mono SC");
        assert_eq!(simplified[3], "Noto Sans Mono CJK SC");
        assert_eq!(simplified[4], "WenQuanYi Zen Hei Mono");

        let traditional = cjk_terminal_fallbacks("zh-Hant");
        assert_eq!(traditional[0], "Sarasa Mono TC");
        assert_eq!(traditional[2], "Noto Sans Mono CJK TC");

        let hong_kong = cjk_terminal_fallbacks("zh_HK.UTF-8");
        assert_eq!(hong_kong[0], "Sarasa Mono HC");
        assert_eq!(hong_kong[2], "Noto Sans Mono CJK HK");
    }

    #[test]
    fn east_asian_locale_selects_matching_glyph_variant() {
        assert_eq!(cjk_terminal_fallbacks("ja_JP")[0], "Sarasa Mono J");
        assert_eq!(cjk_terminal_fallbacks("ko_KR")[0], "D2Coding");
        assert_eq!(
            cjk_terminal_fallbacks("en_US")[0],
            "Maple Mono Normal NF CN"
        );
    }

    #[test]
    fn renderer_uses_the_same_terminal_cell_width_factor_as_layout() {
        assert!((16.0 * TERMINAL_CELL_WIDTH_FACTOR - 9.6).abs() < f32::EPSILON);
    }

    #[test]
    fn transparent_surfaces_prefer_premultiplied_alpha() {
        let modes = [
            CompositeAlphaMode::Opaque,
            CompositeAlphaMode::PreMultiplied,
            CompositeAlphaMode::PostMultiplied,
            CompositeAlphaMode::Inherit,
        ];
        assert_eq!(
            select_alpha_mode(true, &modes),
            CompositeAlphaMode::PreMultiplied
        );
        assert_eq!(select_alpha_mode(false, &modes), CompositeAlphaMode::Opaque);
    }

    #[test]
    fn transparent_surfaces_fall_back_only_to_supported_modes() {
        assert_eq!(
            select_alpha_mode(
                true,
                &[CompositeAlphaMode::Opaque, CompositeAlphaMode::Inherit]
            ),
            CompositeAlphaMode::Inherit
        );
        assert_eq!(
            select_alpha_mode(true, &[CompositeAlphaMode::Opaque]),
            CompositeAlphaMode::Opaque
        );
    }

    #[test]
    fn rectangle_vertices_scale_logical_coordinates_to_physical_pixels() {
        let rect = RectBlock {
            left: 10.0,
            top: 20.0,
            width: 30.0,
            height: 40.0,
            color: [255, 0, 0, 255],
        };
        let logical = rect_vertices(&[rect], 100, 100, 1.0);
        let hidpi = rect_vertices(&[rect], 200, 200, 2.0);
        assert_eq!(logical, hidpi);
        assert_eq!(logical[0].position, [-0.8, 0.6]);
        assert!((logical[2].position[0] + 0.2).abs() < f32::EPSILON * 2.0);
        assert!((logical[2].position[1] + 0.2).abs() < f32::EPSILON * 2.0);
    }

    #[test]
    fn invalid_scale_factors_fall_back_to_one() {
        assert_eq!(normalize_scale_factor(0.0), 1.0);
        assert_eq!(normalize_scale_factor(-2.0), 1.0);
        assert_eq!(normalize_scale_factor(f64::NAN), 1.0);
        assert_eq!(normalize_scale_factor(1.5), 1.5);
    }
}
