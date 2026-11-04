//! Zero-copy video frames: VA-API decodes into GPU surfaces, each surface is
//! exported as DMA-BUF planes, imported into OpenGL through EGL, and converted
//! from YUV to RGB by a shader straight into GPU textures. Pixels never pass
//! through the CPU.

use crate::lock;
use eframe::egui::TextureId;
use eframe::egui_glow::ShaderVersion;
use eframe::glow::{self, HasContext};
use ffmpeg_next as ff;
use std::ffi::{CStr, c_char, c_void};
use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::{Arc, Mutex};

/// GL textures released off the UI thread, deleted on its next frame.
pub type Garbage = Arc<Mutex<Vec<glow::Texture>>>;

/// Handle of the VA-API display a surface belongs to.
#[derive(Clone, Copy)]
pub struct VaDisplay(*mut c_void);

// SAFETY: a VADisplay is a thread-safe libva handle; libva serializes calls.
unsafe impl Send for VaDisplay {}

const VA_STATUS_SUCCESS: i32 = 0;
const VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2: u32 = 0x4000_0000;
const VA_EXPORT_SURFACE_READ_ONLY: u32 = 0x0001;
const VA_EXPORT_SURFACE_SEPARATE_LAYERS: u32 = 0x0004;
const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct DrmObject {
    fd: i32,
    size: u32,
    modifier: u64,
}

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct DrmLayer {
    drm_format: u32,
    num_planes: u32,
    object_index: [u32; 4],
    offset: [u32; 4],
    pitch: [u32; 4],
}

/// libva's VADRMPRIMESurfaceDescriptor.
#[repr(C)]
#[derive(Default)]
struct DrmDescriptor {
    fourcc: u32,
    width: u32,
    height: u32,
    num_objects: u32,
    objects: [DrmObject; 4],
    num_layers: u32,
    layers: [DrmLayer; 4],
}

#[link(name = "va")]
unsafe extern "C" {
    fn vaSyncSurface(display: *mut c_void, surface: u32) -> i32;
    fn vaExportSurfaceHandle(
        display: *mut c_void,
        surface: u32,
        mem_type: u32,
        flags: u32,
        descriptor: *mut c_void,
    ) -> i32;
}

/// The VA-API device a decoder renders into.
pub struct VaDevice(*mut ff::ffi::AVBufferRef);

// SAFETY: the device reference is only created, cloned and freed; FFmpeg's
// hardware device contexts are thread-safe.
unsafe impl Send for VaDevice {}

impl VaDevice {
    /// Open the default render node, or `None` without a usable GPU driver.
    pub fn open() -> Option<Self> {
        let mut device = std::ptr::null_mut();
        // SAFETY: FFmpeg writes a new reference into `device` on success.
        let status = unsafe {
            ff::ffi::av_hwdevice_ctx_create(
                &mut device,
                ff::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI,
                std::ptr::null(),
                std::ptr::null_mut(),
                0,
            )
        };
        (status >= 0 && !device.is_null()).then_some(Self(device))
    }

    /// Whether FFmpeg's decoder for `codec` can decode on a VA-API device.
    pub fn supports(codec: ff::codec::Id) -> bool {
        const HW_DEVICE_CTX: i32 = 0x01;
        // SAFETY: lookups of static codec descriptions.
        unsafe {
            let decoder = ff::ffi::avcodec_find_decoder(codec.into());
            if decoder.is_null() {
                return false;
            }
            let mut index = 0;
            loop {
                let config = ff::ffi::avcodec_get_hw_config(decoder, index);
                if config.is_null() {
                    return false;
                }
                if (*config).device_type == ff::ffi::AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI
                    && (*config).methods & HW_DEVICE_CTX != 0
                {
                    return true;
                }
                index += 1;
            }
        }
    }

    /// Make `decoder` decode into this device's surfaces when it supports the
    /// codec. `surfaces` frames may be held outside the decoder at once.
    pub fn attach(&self, decoder: &mut ff::codec::Context, surfaces: i32) {
        // SAFETY: plain field writes on a codec context that isn't open yet;
        // the decoder takes its own reference to the device.
        unsafe {
            let context = decoder.as_mut_ptr();
            (*context).hw_device_ctx = ff::ffi::av_buffer_ref(self.0);
            (*context).get_format = Some(prefer_vaapi);
            (*context).extra_hw_frames = surfaces;
        }
    }

    /// The libva display of this device.
    pub fn display(&self) -> VaDisplay {
        // SAFETY: a VAAPI device context's hwctx is an AVVAAPIDeviceContext.
        unsafe {
            let device = (*self.0).data.cast::<ff::ffi::AVHWDeviceContext>();
            let hwctx = (*device).hwctx.cast::<ff::ffi::AVVAAPIDeviceContext>();
            VaDisplay((*hwctx).display)
        }
    }
}

impl Drop for VaDevice {
    fn drop(&mut self) {
        // SAFETY: releases the reference from open().
        unsafe { ff::ffi::av_buffer_unref(&mut self.0) };
    }
}

/// Pick the VA-API surface format when the decoder offers it for this
/// stream, else its first (software) format.
unsafe extern "C" fn prefer_vaapi(
    _context: *mut ff::ffi::AVCodecContext,
    formats: *const ff::ffi::AVPixelFormat,
) -> ff::ffi::AVPixelFormat {
    let mut index = 0;
    // SAFETY: FFmpeg passes a list terminated by AV_PIX_FMT_NONE.
    unsafe {
        loop {
            let format = *formats.add(index);
            if format == ff::ffi::AVPixelFormat::AV_PIX_FMT_NONE {
                return *formats;
            }
            if format == ff::ffi::AVPixelFormat::AV_PIX_FMT_VAAPI {
                return format;
            }
            index += 1;
        }
    }
}

/// Whether `frame` lives in a VA-API surface.
pub fn is_hardware(frame: &ff::frame::Video) -> bool {
    // SAFETY: plain field read on a valid frame.
    unsafe { (*frame.as_ptr()).format == ff::ffi::AVPixelFormat::AV_PIX_FMT_VAAPI as i32 }
}

/// Copy a VA-API frame into system memory (the fallback when the GPU path
/// fails). Returns false on failure.
pub fn download(hardware: &ff::frame::Video, software: &mut ff::frame::Video) -> bool {
    // SAFETY: both frames are valid; FFmpeg allocates the software frame.
    unsafe {
        ff::ffi::av_frame_unref(software.as_mut_ptr());
        ff::ffi::av_hwframe_transfer_data(software.as_mut_ptr(), hardware.as_ptr(), 0) >= 0
    }
}

/// A decoded frame in a VA-API surface, to be drawn at `width`×`height`.
pub struct VaFrame {
    frame: ff::frame::Video,
    display: VaDisplay,
    pub width: u32,
    pub height: u32,
}

impl VaFrame {
    /// Wait for the GPU to finish decoding `frame`; `None` if it failed.
    pub fn new(
        frame: ff::frame::Video,
        display: VaDisplay,
        width: u32,
        height: u32,
    ) -> Option<Self> {
        let surface = surface_id(&frame);
        // SAFETY: the surface belongs to `display` and is kept alive by `frame`.
        let status = unsafe { vaSyncSurface(display.0, surface) };
        (status == VA_STATUS_SUCCESS).then_some(Self {
            frame,
            display,
            width,
            height,
        })
    }

    /// Bytes of the surface, for queue budgets.
    pub fn bytes(&self) -> usize {
        self.frame.width() as usize * self.frame.height() as usize * 3 / 2
    }
}

fn surface_id(frame: &ff::frame::Video) -> u32 {
    // SAFETY: a VAAPI frame stores its VASurfaceID in data[3].
    unsafe { (*frame.as_ptr()).data[3] as usize as u32 }
}

type CreateImage =
    unsafe extern "C" fn(*mut c_void, *mut c_void, u32, *mut c_void, *const i32) -> *mut c_void;
type DestroyImage = unsafe extern "C" fn(*mut c_void, *mut c_void) -> u32;
type ImageTargetTexture = unsafe extern "C" fn(u32, *mut c_void);

/// EGL entry points for importing DMA-BUFs, loaded at runtime.
struct Egl {
    _library: libloading::Library,
    display: *mut c_void,
    create_image: CreateImage,
    destroy_image: DestroyImage,
    image_target_texture: ImageTargetTexture,
}

const EGL_EXTENSIONS: i32 = 0x3055;
const EGL_NONE: i32 = 0x3038;
const EGL_WIDTH: i32 = 0x3057;
const EGL_HEIGHT: i32 = 0x3056;
const EGL_LINUX_DMA_BUF_EXT: u32 = 0x3270;
const EGL_LINUX_DRM_FOURCC_EXT: i32 = 0x3271;
const EGL_DMA_BUF_PLANE0_FD_EXT: i32 = 0x3272;
const EGL_DMA_BUF_PLANE0_OFFSET_EXT: i32 = 0x3273;
const EGL_DMA_BUF_PLANE0_PITCH_EXT: i32 = 0x3274;
const EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT: i32 = 0x3443;
const EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT: i32 = 0x3444;

impl Egl {
    /// EGL of the current GL context, if it is EGL and can import DMA-BUFs.
    fn load() -> Result<Self, String> {
        type GetProcAddress = unsafe extern "C" fn(*const c_char) -> *mut c_void;
        type GetCurrentDisplay = unsafe extern "C" fn() -> *mut c_void;
        type QueryString = unsafe extern "C" fn(*mut c_void, i32) -> *const c_char;
        // SAFETY: libEGL is a system library with a stable C ABI; the symbol
        // types match the EGL 1.4 specification.
        unsafe {
            let library = libloading::Library::new("libEGL.so.1")
                .map_err(|error| format!("load libEGL: {error}"))?;
            let get_proc: libloading::Symbol<GetProcAddress> = library
                .get(b"eglGetProcAddress\0")
                .map_err(|error| error.to_string())?;
            let current: libloading::Symbol<GetCurrentDisplay> = library
                .get(b"eglGetCurrentDisplay\0")
                .map_err(|error| error.to_string())?;
            let query: libloading::Symbol<QueryString> = library
                .get(b"eglQueryString\0")
                .map_err(|error| error.to_string())?;
            let display = current();
            if display.is_null() {
                return Err("the GL context is not EGL".into());
            }
            let extensions = query(display, EGL_EXTENSIONS);
            if extensions.is_null() {
                return Err("no EGL extensions".into());
            }
            let extensions = CStr::from_ptr(extensions).to_string_lossy();
            for needed in ["EGL_EXT_image_dma_buf_import", "EGL_KHR_image_base"] {
                if !extensions.split(' ').any(|name| name == needed) {
                    return Err(format!("EGL lacks {needed}"));
                }
            }
            let lookup = |name: &[u8]| {
                let address = get_proc(name.as_ptr().cast());
                (!address.is_null()).then_some(address)
            };
            let missing = || "EGL image functions missing".to_string();
            let create_image = lookup(b"eglCreateImageKHR\0").ok_or_else(missing)?;
            let destroy_image = lookup(b"eglDestroyImageKHR\0").ok_or_else(missing)?;
            let image_target = lookup(b"glEGLImageTargetTexture2DOES\0").ok_or_else(missing)?;
            Ok(Self {
                display,
                create_image: std::mem::transmute::<*mut c_void, CreateImage>(create_image),
                destroy_image: std::mem::transmute::<*mut c_void, DestroyImage>(destroy_image),
                image_target_texture: std::mem::transmute::<*mut c_void, ImageTargetTexture>(
                    image_target,
                ),
                _library: library,
            })
        }
    }
}

const VERTEX_SHADER: &str = r#"
uniform vec2 u_crop;
out vec2 v_uv;
void main() {
    // One triangle covering the target; corners at 0 and 2.
    vec2 corner = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
    v_uv = corner * u_crop;
    gl_Position = vec4(corner * 2.0 - 1.0, 0.0, 1.0);
}
"#;

const FRAGMENT_SHADER: &str = r#"
#ifdef GL_ES
precision highp float;
#endif
uniform sampler2D u_luma;
uniform sampler2D u_chroma;
uniform mat3 u_matrix;
uniform vec3 u_offset;
in vec2 v_uv;
out vec4 f_color;
void main() {
    vec3 yuv = vec3(texture(u_luma, v_uv).r, texture(u_chroma, v_uv).rg);
    f_color = vec4(u_matrix * (yuv - u_offset), 1.0);
}
"#;

/// Converts VA-API frames into GL textures and owns the texture egui shows
/// video through.
pub struct Gpu {
    gl: Arc<glow::Context>,
    egl: Egl,
    program: glow::Program,
    vertex_array: glow::VertexArray,
    framebuffer: glow::Framebuffer,
    read_framebuffer: glow::Framebuffer,
    crop: Option<glow::UniformLocation>,
    matrix: Option<glow::UniformLocation>,
    offset: Option<glow::UniformLocation>,
    /// The texture registered with egui, its id, and its current size.
    screen: Option<(glow::Texture, TextureId)>,
    screen_size: [u32; 2],
    garbage: Garbage,
    /// Frames the GPU may still be reading, kept alive (so the decoder can't
    /// reuse their surfaces) until their fence signals.
    in_flight: Vec<(glow::Fence, VaFrame)>,
    /// Set once an import fails; players then decode for the CPU.
    pub failed: bool,
}

impl Gpu {
    /// Set up on the current GL context, or explain why zero-copy video
    /// isn't available here.
    pub fn new(gl: Arc<glow::Context>) -> Result<Self, String> {
        let egl = Egl::load()?;
        let version = ShaderVersion::get(&gl);
        if !version.is_new_shader_interface() {
            return Err("GLSL too old for the video shader".into());
        }
        // SAFETY: GL calls on the current context with valid arguments.
        unsafe {
            let program = gl.create_program()?;
            for (kind, source) in [
                (glow::VERTEX_SHADER, VERTEX_SHADER),
                (glow::FRAGMENT_SHADER, FRAGMENT_SHADER),
            ] {
                let shader = gl.create_shader(kind)?;
                let header = version.version_declaration();
                gl.shader_source(shader, &format!("{header}\n{source}"));
                gl.compile_shader(shader);
                if !gl.get_shader_compile_status(shader) {
                    return Err(gl.get_shader_info_log(shader));
                }
                gl.attach_shader(program, shader);
                gl.delete_shader(shader);
            }
            gl.link_program(program);
            if !gl.get_program_link_status(program) {
                return Err(gl.get_program_info_log(program));
            }
            gl.use_program(Some(program));
            let luma = gl.get_uniform_location(program, "u_luma");
            let chroma = gl.get_uniform_location(program, "u_chroma");
            gl.uniform_1_i32(luma.as_ref(), 0);
            gl.uniform_1_i32(chroma.as_ref(), 1);
            gl.use_program(None);
            Ok(Self {
                crop: gl.get_uniform_location(program, "u_crop"),
                matrix: gl.get_uniform_location(program, "u_matrix"),
                offset: gl.get_uniform_location(program, "u_offset"),
                vertex_array: gl.create_vertex_array()?,
                framebuffer: gl.create_framebuffer()?,
                read_framebuffer: gl.create_framebuffer()?,
                program,
                egl,
                gl,
                screen: None,
                screen_size: [0, 0],
                garbage: Garbage::default(),
                in_flight: Vec::new(),
                failed: false,
            })
        }
    }

    /// Create the screen texture and register it with egui (once).
    pub fn register(&mut self, register: impl FnOnce(glow::Texture) -> TextureId) {
        if self.screen.is_some() {
            return;
        }
        // SAFETY: GL call on the current context.
        if let Ok(texture) = unsafe { self.gl.create_texture() } {
            self.allocate(texture, 2, 2);
            self.screen = Some((texture, register(texture)));
            self.screen_size = [2, 2];
        }
    }

    /// The egui id of the screen texture.
    pub fn screen_id(&self) -> Option<TextureId> {
        self.screen.map(|(_, id)| id)
    }

    /// Where players release textures from any thread.
    pub fn garbage(&self) -> Garbage {
        self.garbage.clone()
    }

    /// Release frames the GPU has finished reading.
    fn retire(&mut self) {
        let gl = &self.gl;
        self.in_flight.retain(|(fence, _)| {
            // SAFETY: GL calls on the current context with a live fence.
            unsafe {
                let status = gl.client_wait_sync(*fence, 0, 0);
                let busy = status == glow::TIMEOUT_EXPIRED;
                if !busy {
                    gl.delete_sync(*fence);
                }
                busy
            }
        });
    }

    /// Delete textures released since the last frame, and release frames
    /// the GPU is done with.
    pub fn collect_garbage(&mut self) {
        self.retire();
        for texture in lock(&self.garbage).drain(..) {
            // SAFETY: the texture was created by this context and is unused.
            unsafe { self.gl.delete_texture(texture) };
        }
    }

    /// A new RGBA texture of `width`×`height`.
    pub fn new_texture(&self, width: u32, height: u32) -> Result<glow::Texture, String> {
        // SAFETY: GL call on the current context.
        let texture = unsafe { self.gl.create_texture()? };
        self.allocate(texture, width, height);
        Ok(texture)
    }

    fn allocate(&self, texture: glow::Texture, width: u32, height: u32) {
        let gl = &self.gl;
        // SAFETY: GL calls on the current context with valid arguments.
        unsafe {
            gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA8 as i32,
                width as i32,
                height as i32,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(None),
            );
            for (parameter, value) in [
                (glow::TEXTURE_MIN_FILTER, glow::LINEAR),
                (glow::TEXTURE_MAG_FILTER, glow::LINEAR),
                (glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE),
                (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE),
            ] {
                gl.tex_parameter_i32(glow::TEXTURE_2D, parameter, value as i32);
            }
            gl.bind_texture(glow::TEXTURE_2D, None);
        }
    }

    /// Resize the screen texture to `width`×`height` if needed.
    fn fit_screen(&mut self, width: u32, height: u32) -> Option<glow::Texture> {
        let (texture, _) = self.screen?;
        if self.screen_size != [width, height] {
            self.allocate(texture, width, height);
            self.screen_size = [width, height];
        }
        Some(texture)
    }

    /// Draw `frame` onto the screen texture.
    pub fn show_frame(&mut self, frame: VaFrame) -> Result<(), String> {
        let screen = self
            .fit_screen(frame.width, frame.height)
            .ok_or("no screen texture")?;
        let (width, height) = (frame.width, frame.height);
        self.convert(frame, screen, width, height)
    }

    /// Copy a converted cache texture of `width`×`height` onto the screen.
    pub fn show_texture(&mut self, texture: glow::Texture, width: u32, height: u32) {
        let Some(screen) = self.fit_screen(width, height) else {
            return;
        };
        let gl = &self.gl;
        let (width, height) = (width as i32, height as i32);
        // SAFETY: GL calls on the current context; both textures are complete
        // RGBA8 textures of the same size.
        unsafe {
            gl.bind_framebuffer(glow::READ_FRAMEBUFFER, Some(self.read_framebuffer));
            gl.framebuffer_texture_2d(
                glow::READ_FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(texture),
                0,
            );
            gl.bind_framebuffer(glow::DRAW_FRAMEBUFFER, Some(self.framebuffer));
            gl.framebuffer_texture_2d(
                glow::DRAW_FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(screen),
                0,
            );
            gl.disable(glow::SCISSOR_TEST);
            gl.blit_framebuffer(
                0,
                0,
                width,
                height,
                0,
                0,
                width,
                height,
                glow::COLOR_BUFFER_BIT,
                glow::NEAREST,
            );
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        }
    }

    /// Convert `frame` to RGB into `target`, scaled to `width`×`height`.
    pub fn convert(
        &mut self,
        frame: VaFrame,
        target: glow::Texture,
        width: u32,
        height: u32,
    ) -> Result<(), String> {
        self.retire();
        let mut descriptor = DrmDescriptor::default();
        // SAFETY: the surface is alive while `frame` is; libva fills the
        // descriptor and hands us ownership of its file descriptors.
        let status = unsafe {
            vaExportSurfaceHandle(
                frame.display.0,
                surface_id(&frame.frame),
                VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2,
                VA_EXPORT_SURFACE_READ_ONLY | VA_EXPORT_SURFACE_SEPARATE_LAYERS,
                (&mut descriptor as *mut DrmDescriptor).cast(),
            )
        };
        if status != VA_STATUS_SUCCESS {
            return Err(format!("vaExportSurfaceHandle failed ({status})"));
        }
        let count = (descriptor.num_objects as usize).min(4);
        // Closed on return; EGL keeps its own references to the buffers.
        // SAFETY: libva gave us these descriptors to own.
        let _fds: Vec<OwnedFd> = descriptor.objects[..count]
            .iter()
            .map(|object| unsafe { OwnedFd::from_raw_fd(object.fd) })
            .collect();
        if descriptor.num_layers < 2 {
            return Err(format!(
                "unexpected surface layout {:#x}",
                descriptor.fourcc
            ));
        }
        let luma = self.import(&descriptor, 0, descriptor.width, descriptor.height)?;
        let chroma = self.import(
            &descriptor,
            1,
            descriptor.width.div_ceil(2),
            descriptor.height.div_ceil(2),
        );
        let result = chroma.map(|chroma| {
            let crop = [
                frame.frame.width() as f32 / descriptor.width as f32,
                frame.frame.height() as f32 / descriptor.height as f32,
            ];
            let colors = colors(&frame.frame);
            self.draw(&luma, &chroma, crop, colors, target, width, height);
            self.release(chroma);
        });
        self.release(luma);
        result?;
        // SAFETY: GL calls on the current context.
        let fence = unsafe { self.gl.fence_sync(glow::SYNC_GPU_COMMANDS_COMPLETE, 0)? };
        // SAFETY: as above; submits the draw so the fence can signal.
        unsafe { self.gl.flush() };
        self.in_flight.push((fence, frame));
        Ok(())
    }

    /// Import layer `index` of `descriptor` as a GL texture.
    fn import(
        &self,
        descriptor: &DrmDescriptor,
        index: usize,
        width: u32,
        height: u32,
    ) -> Result<Imported, String> {
        let layer = descriptor.layers[index];
        let object = descriptor.objects[layer.object_index[0] as usize];
        let mut attributes = vec![
            EGL_WIDTH,
            width as i32,
            EGL_HEIGHT,
            height as i32,
            EGL_LINUX_DRM_FOURCC_EXT,
            layer.drm_format as i32,
            EGL_DMA_BUF_PLANE0_FD_EXT,
            object.fd,
            EGL_DMA_BUF_PLANE0_OFFSET_EXT,
            layer.offset[0] as i32,
            EGL_DMA_BUF_PLANE0_PITCH_EXT,
            layer.pitch[0] as i32,
        ];
        if object.modifier != DRM_FORMAT_MOD_INVALID {
            attributes.extend([
                EGL_DMA_BUF_PLANE0_MODIFIER_LO_EXT,
                object.modifier as u32 as i32,
                EGL_DMA_BUF_PLANE0_MODIFIER_HI_EXT,
                (object.modifier >> 32) as u32 as i32,
            ]);
        }
        attributes.push(EGL_NONE);
        // SAFETY: valid display and attribute list; a null context and
        // buffer are what EGL_LINUX_DMA_BUF_EXT requires.
        let image = unsafe {
            (self.egl.create_image)(
                self.egl.display,
                std::ptr::null_mut(),
                EGL_LINUX_DMA_BUF_EXT,
                std::ptr::null_mut(),
                attributes.as_ptr(),
            )
        };
        if image.is_null() {
            return Err(format!(
                "EGL import of format {:#x} modifier {:#x} failed",
                layer.drm_format, object.modifier
            ));
        }
        // SAFETY: GL calls on the current context; the image is valid.
        unsafe {
            let texture = self.gl.create_texture().inspect_err(|_| {
                (self.egl.destroy_image)(self.egl.display, image);
            })?;
            self.gl.bind_texture(glow::TEXTURE_2D, Some(texture));
            (self.egl.image_target_texture)(glow::TEXTURE_2D, image);
            for parameter in [glow::TEXTURE_MIN_FILTER, glow::TEXTURE_MAG_FILTER] {
                self.gl
                    .tex_parameter_i32(glow::TEXTURE_2D, parameter, glow::LINEAR as i32);
            }
            for parameter in [glow::TEXTURE_WRAP_S, glow::TEXTURE_WRAP_T] {
                self.gl
                    .tex_parameter_i32(glow::TEXTURE_2D, parameter, glow::CLAMP_TO_EDGE as i32);
            }
            Ok(Imported { image, texture })
        }
    }

    fn release(&self, imported: Imported) {
        // SAFETY: both were created by import() and are no longer used.
        unsafe {
            self.gl.delete_texture(imported.texture);
            (self.egl.destroy_image)(self.egl.display, imported.image);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn draw(
        &self,
        luma: &Imported,
        chroma: &Imported,
        crop: [f32; 2],
        colors: Colors,
        target: glow::Texture,
        width: u32,
        height: u32,
    ) {
        let gl = &self.gl;
        // SAFETY: GL calls on the current context with valid objects. State
        // egui relies on (framebuffer, texture unit, program) is restored;
        // egui sets everything else itself before it paints.
        unsafe {
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.framebuffer));
            gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(target),
                0,
            );
            gl.viewport(0, 0, width as i32, height as i32);
            gl.disable(glow::SCISSOR_TEST);
            gl.disable(glow::BLEND);
            gl.use_program(Some(self.program));
            gl.uniform_2_f32(self.crop.as_ref(), crop[0], crop[1]);
            gl.uniform_matrix_3_f32_slice(self.matrix.as_ref(), false, &colors.matrix);
            let [y, u, v] = colors.offset;
            gl.uniform_3_f32(self.offset.as_ref(), y, u, v);
            gl.active_texture(glow::TEXTURE1);
            gl.bind_texture(glow::TEXTURE_2D, Some(chroma.texture));
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(luma.texture));
            gl.bind_vertex_array(Some(self.vertex_array));
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
            gl.bind_vertex_array(None);
            gl.bind_texture(glow::TEXTURE_2D, None);
            gl.use_program(None);
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        }
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        // SAFETY: waits for all GPU work before frames are released.
        unsafe { self.gl.finish() };
        self.collect_garbage();
        // SAFETY: deletes objects this context created; the screen texture
        // belongs to egui's painter once registered.
        unsafe {
            self.gl.delete_program(self.program);
            self.gl.delete_vertex_array(self.vertex_array);
            self.gl.delete_framebuffer(self.framebuffer);
            self.gl.delete_framebuffer(self.read_framebuffer);
        }
    }
}

/// A DMA-BUF plane imported as a GL texture.
struct Imported {
    image: *mut c_void,
    texture: glow::Texture,
}

/// YUV to RGB conversion for the shader: rgb = matrix * (yuv - offset).
struct Colors {
    matrix: [f32; 9],
    offset: [f32; 3],
}

/// Conversion for the frame's colour space and range. Unspecified HD video
/// is BT.709, SD is BT.601, as players assume.
fn colors(frame: &ff::frame::Video) -> Colors {
    use ff::ffi::{AVColorRange, AVColorSpace};
    // SAFETY: plain field reads on a valid frame.
    let (space, range) = unsafe { ((*frame.as_ptr()).colorspace, (*frame.as_ptr()).color_range) };
    let (red, blue) = match space {
        AVColorSpace::AVCOL_SPC_BT470BG
        | AVColorSpace::AVCOL_SPC_SMPTE170M
        | AVColorSpace::AVCOL_SPC_SMPTE240M => (0.299, 0.114),
        AVColorSpace::AVCOL_SPC_BT2020_NCL | AVColorSpace::AVCOL_SPC_BT2020_CL => (0.2627, 0.0593),
        AVColorSpace::AVCOL_SPC_BT709 => (0.2126, 0.0722),
        _ if frame.height() >= 720 => (0.2126, 0.0722),
        _ => (0.299, 0.114),
    };
    let green = 1.0 - red - blue;
    let full = range == AVColorRange::AVCOL_RANGE_JPEG;
    let (luma_scale, chroma_scale, black) = if full {
        (1.0, 1.0, 0.0)
    } else {
        (255.0 / 219.0, 255.0 / 224.0, 16.0 / 255.0)
    };
    // Columns: contribution of Y, Cb and Cr to (R, G, B).
    let luma = [luma_scale; 3];
    let cb = [
        0.0,
        -2.0 * blue * (1.0 - blue) / green * chroma_scale,
        2.0 * (1.0 - blue) * chroma_scale,
    ];
    let cr = [
        2.0 * (1.0 - red) * chroma_scale,
        -2.0 * red * (1.0 - red) / green * chroma_scale,
        0.0,
    ];
    Colors {
        matrix: [
            luma[0], luma[1], luma[2], cb[0], cb[1], cb[2], cr[0], cr[1], cr[2],
        ],
        offset: [black, 128.0 / 255.0, 128.0 / 255.0],
    }
}

#[cfg(test)]
impl Gpu {
    /// The screen texture's pixels (RGBA rows, bottom-up as GL reads them)
    /// and size.
    pub fn read_screen(&self) -> (Vec<u8>, [u32; 2]) {
        let [width, height] = self.screen_size;
        let mut pixels = vec![0; width as usize * height as usize * 4];
        if let Some((texture, _)) = self.screen {
            // SAFETY: GL calls on the current context; the buffer fits.
            unsafe {
                let gl = &self.gl;
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(self.read_framebuffer));
                gl.framebuffer_texture_2d(
                    glow::FRAMEBUFFER,
                    glow::COLOR_ATTACHMENT0,
                    glow::TEXTURE_2D,
                    Some(texture),
                    0,
                );
                gl.read_pixels(
                    0,
                    0,
                    width as i32,
                    height as i32,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    glow::PixelPackData::Slice(Some(&mut pixels)),
                );
                gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            }
        }
        (pixels, self.screen_size)
    }
}

/// A windowless EGL context for running the GPU path under `cargo test`.
#[cfg(test)]
pub mod headless {
    use super::*;

    const EGL_PLATFORM_SURFACELESS_MESA: u32 = 0x31DD;
    const EGL_OPENGL_API: u32 = 0x30A2;
    const EGL_CONTEXT_MAJOR_VERSION: i32 = 0x3098;
    const EGL_CONTEXT_MINOR_VERSION: i32 = 0x30FB;
    const EGL_CONTEXT_OPENGL_PROFILE_MASK: i32 = 0x30FD;

    /// Keeps libEGL loaded while the context is in use.
    pub struct Context {
        _library: libloading::Library,
    }

    /// Make a GL 3.3 core context current on this thread.
    pub fn context() -> (Context, Arc<glow::Context>) {
        type GetPlatformDisplay =
            unsafe extern "C" fn(u32, *mut c_void, *const isize) -> *mut c_void;
        type Initialize = unsafe extern "C" fn(*mut c_void, *mut i32, *mut i32) -> u32;
        type BindApi = unsafe extern "C" fn(u32) -> u32;
        type CreateContext =
            unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *const i32) -> *mut c_void;
        type MakeCurrent =
            unsafe extern "C" fn(*mut c_void, *mut c_void, *mut c_void, *mut c_void) -> u32;
        type GetProcAddress = unsafe extern "C" fn(*const c_char) -> *mut c_void;
        // SAFETY: test-only EGL setup following the EGL 1.5 specification.
        unsafe {
            let library = libloading::Library::new("libEGL.so.1").expect("libEGL");
            let display = library
                .get::<GetPlatformDisplay>(b"eglGetPlatformDisplay\0")
                .unwrap()(
                EGL_PLATFORM_SURFACELESS_MESA,
                std::ptr::null_mut(),
                std::ptr::null(),
            );
            assert!(!display.is_null(), "no surfaceless EGL display");
            let (mut major, mut minor) = (0, 0);
            assert_eq!(
                library.get::<Initialize>(b"eglInitialize\0").unwrap()(
                    display, &mut major, &mut minor
                ),
                1
            );
            library.get::<BindApi>(b"eglBindAPI\0").unwrap()(EGL_OPENGL_API);
            let attributes = [
                EGL_CONTEXT_MAJOR_VERSION,
                3,
                EGL_CONTEXT_MINOR_VERSION,
                3,
                EGL_CONTEXT_OPENGL_PROFILE_MASK,
                1,
                EGL_NONE,
            ];
            let context = library.get::<CreateContext>(b"eglCreateContext\0").unwrap()(
                display,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                attributes.as_ptr(),
            );
            assert!(!context.is_null(), "no EGL context");
            let made = library.get::<MakeCurrent>(b"eglMakeCurrent\0").unwrap()(
                display,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                context,
            );
            assert_eq!(made, 1);
            let get_proc = *library
                .get::<GetProcAddress>(b"eglGetProcAddress\0")
                .unwrap();
            let gl = glow::Context::from_loader_function(|name| {
                let name = std::ffi::CString::new(name).unwrap();
                get_proc(name.as_ptr()).cast_const()
            });
            (Context { _library: library }, Arc::new(gl))
        }
    }
}

#[cfg(test)]
mod tests {
    /// Needs a VA-API driver: cargo test --release va_device -- --ignored
    #[test]
    #[ignore]
    fn va_device_decodes_hevc() {
        ffmpeg_next::init().unwrap();
        assert!(super::VaDevice::supports(ffmpeg_next::codec::Id::HEVC));
        assert!(super::VaDevice::open().is_some(), "no VA-API device");
    }
}
