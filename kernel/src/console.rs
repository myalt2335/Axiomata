#![allow(dead_code)]
#![allow(unused_variables)]

extern crate alloc;
use alloc::vec::Vec;
use bootloader_api::info::{FrameBufferInfo, PixelFormat};
use bootloader_api::BootInfo;
use core::mem::MaybeUninit;
#[cfg(target_arch = "x86_64")]
use core::arch::x86_64::*;
use core::ptr::addr_of_mut;
use spin::Mutex;
use x86_64::instructions::interrupts;
use crate::font::VGA8_FONT;
use crate::font2::TERMINUS_FONT;
use crate::font3::SPLEEN_FONT;
use crate::memory;
use crate::wait;

#[derive(Copy, Clone)]
struct Font {
    glyph: fn(u8) -> &'static [u8],
    width: usize,
    height: usize,
    name: &'static str,
}

impl Font {
    fn glyph(&self, c: char) -> &'static [u8] {
        let code = c as u8;
        let idx = if code < 0x20 || code > 0x7e { 0 } else { (code - 0x20) as usize };
        (self.glyph)(idx as u8)
    }
}

fn write_pixel_raw_format(buf: &mut [u8], off: usize, color: u32, pixel_format: PixelFormat, bpp: usize) {
    if off + bpp > buf.len() {
        return;
    }
    let r = ((color >> 16) & 0xFF) as u8;
    let g = ((color >> 8) & 0xFF) as u8;
    let b = (color & 0xFF) as u8;
    match (pixel_format, bpp) {
        (PixelFormat::Rgb, 4) => {
            buf[off] = r;
            buf[off + 1] = g;
            buf[off + 2] = b;
            buf[off + 3] = 0xFF;
        }
        (PixelFormat::Rgb, 3) => {
            buf[off] = r;
            buf[off + 1] = g;
            buf[off + 2] = b;
        }
        (PixelFormat::Bgr, 4) => {
            buf[off] = b;
            buf[off + 1] = g;
            buf[off + 2] = r;
            buf[off + 3] = 0xFF;
        }
        (PixelFormat::Bgr, 3) => {
            buf[off] = b;
            buf[off + 1] = g;
            buf[off + 2] = r;
        }
        _ => {}
    }
}

fn read_pixel_raw_format(buf: &[u8], off: usize, pixel_format: PixelFormat, bpp: usize) -> u32 {
    if off + bpp > buf.len() {
        return 0;
    }
    match (pixel_format, bpp) {
        (PixelFormat::Rgb, 4) => {
            let r = buf[off] as u32;
            let g = buf[off + 1] as u32;
            let b = buf[off + 2] as u32;
            (r << 16) | (g << 8) | b
        }
        (PixelFormat::Rgb, 3) => {
            let r = buf[off] as u32;
            let g = buf[off + 1] as u32;
            let b = buf[off + 2] as u32;
            (r << 16) | (g << 8) | b
        }
        (PixelFormat::Bgr, 4) => {
            let b = buf[off] as u32;
            let g = buf[off + 1] as u32;
            let r = buf[off + 2] as u32;
            (r << 16) | (g << 8) | b
        }
        (PixelFormat::Bgr, 3) => {
            let b = buf[off] as u32;
            let g = buf[off + 1] as u32;
            let r = buf[off + 2] as u32;
            (r << 16) | (g << 8) | b
        }
        _ => 0,
    }
}

const SSE2_MIN_BYTES: usize = 256;

fn copy_row_opaque_scalar(dst: &mut [u8], src: &[u8]) {
    debug_assert_eq!(dst.len(), src.len());
    let mut i = 0;
    while i + 4 <= dst.len() {
        dst[i] = src[i];
        dst[i + 1] = src[i + 1];
        dst[i + 2] = src[i + 2];
        dst[i + 3] = 0xFF;
        i += 4;
    }
}

fn blend_row_alpha_scalar(dst: &mut [u8], src: &[u8], alpha: u8) {
    debug_assert_eq!(dst.len(), src.len());
    let alpha = alpha as u32;
    let inv = 255u32 - alpha;
    let mut i = 0;
    while i + 4 <= dst.len() {
        let sr = src[i] as u32;
        let sg = src[i + 1] as u32;
        let sb = src[i + 2] as u32;
        let dr = dst[i] as u32;
        let dg = dst[i + 1] as u32;
        let db = dst[i + 2] as u32;
        dst[i] = ((sr * alpha + dr * inv) / 255) as u8;
        dst[i + 1] = ((sg * alpha + dg * inv) / 255) as u8;
        dst[i + 2] = ((sb * alpha + db * inv) / 255) as u8;
        dst[i + 3] = 0xFF;
        i += 4;
    }
}

#[cfg(all(target_arch = "x86_64", target_feature = "sse2"))]
#[target_feature(enable = "sse2")]
unsafe fn copy_row_opaque_sse2(dst: &mut [u8], src: &[u8]) {
    debug_assert_eq!(dst.len(), src.len());
    let len = dst.len();
    let alpha_mask = _mm_set1_epi32(0xFF000000u32 as i32);
    let mut i = 0;
    while i + 16 <= len {
        let v = _mm_loadu_si128(src.as_ptr().add(i) as *const __m128i);
        let v = _mm_or_si128(v, alpha_mask);
        _mm_storeu_si128(dst.as_mut_ptr().add(i) as *mut __m128i, v);
        i += 16;
    }
    if i < len {
        copy_row_opaque_scalar(&mut dst[i..], &src[i..]);
    }
}

#[cfg(all(target_arch = "x86_64", target_feature = "sse2"))]
#[target_feature(enable = "sse2")]
unsafe fn blend_row_alpha_sse2(dst: &mut [u8], src: &[u8], alpha: u8) {
    debug_assert_eq!(dst.len(), src.len());
    let len = dst.len();
    let alpha16 = _mm_set1_epi16(alpha as i16);
    let inv16 = _mm_set1_epi16((255u16 - alpha as u16) as i16);
    let one = _mm_set1_epi16(1);
    let zero = _mm_setzero_si128();
    let alpha_mask = _mm_set1_epi32(0xFF000000u32 as i32);
    let mut i = 0;
    while i + 16 <= len {
        let s = _mm_loadu_si128(src.as_ptr().add(i) as *const __m128i);
        let d = _mm_loadu_si128(dst.as_ptr().add(i) as *const __m128i);
        let s_lo = _mm_unpacklo_epi8(s, zero);
        let s_hi = _mm_unpackhi_epi8(s, zero);
        let d_lo = _mm_unpacklo_epi8(d, zero);
        let d_hi = _mm_unpackhi_epi8(d, zero);
        let s_lo = _mm_mullo_epi16(s_lo, alpha16);
        let s_hi = _mm_mullo_epi16(s_hi, alpha16);
        let d_lo = _mm_mullo_epi16(d_lo, inv16);
        let d_hi = _mm_mullo_epi16(d_hi, inv16);
        let sum_lo = _mm_add_epi16(s_lo, d_lo);
        let sum_hi = _mm_add_epi16(s_hi, d_hi);
        let sum_lo_shift = _mm_srli_epi16(sum_lo, 8);
        let sum_hi_shift = _mm_srli_epi16(sum_hi, 8);
        let sum_lo = _mm_add_epi16(_mm_add_epi16(sum_lo, one), sum_lo_shift);
        let sum_hi = _mm_add_epi16(_mm_add_epi16(sum_hi, one), sum_hi_shift);
        let out_lo = _mm_srli_epi16(sum_lo, 8);
        let out_hi = _mm_srli_epi16(sum_hi, 8);
        let out = _mm_packus_epi16(out_lo, out_hi);
        let out = _mm_or_si128(out, alpha_mask);
        _mm_storeu_si128(dst.as_mut_ptr().add(i) as *mut __m128i, out);
        i += 16;
    }
    if i < len {
        blend_row_alpha_scalar(&mut dst[i..], &src[i..], alpha);
    }
}

fn copy_row_opaque_4bpp(dst: &mut [u8], src: &[u8]) {
    debug_assert_eq!(dst.len(), src.len());
    dst.copy_from_slice(src);
}

fn blend_row_alpha_4bpp(dst: &mut [u8], src: &[u8], alpha: u8) {
    if dst.len() < SSE2_MIN_BYTES {
        blend_row_alpha_scalar(dst, src, alpha);
        return;
    }
    #[cfg(all(target_arch = "x86_64", target_feature = "sse2"))]
    unsafe {
        blend_row_alpha_sse2(dst, src, alpha);
    }
    #[cfg(not(all(target_arch = "x86_64", target_feature = "sse2")))]
    {
        blend_row_alpha_scalar(dst, src, alpha);
    }
}

fn merge_dirty_rect(dst: &mut Option<(usize, usize, usize, usize)>, x0: usize, y0: usize, x1: usize, y1: usize) {
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    match *dst {
        Some((dx0, dy0, dx1, dy1)) => {
            *dst = Some((dx0.min(x0), dy0.min(y0), dx1.max(x1), dy1.max(y1)));
        }
        None => {
            *dst = Some((x0, y0, x1, y1));
        }
    }
}

#[derive(Copy, Clone)]
struct DirtyRect {
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
}

impl DirtyRect {
    const fn new(x0: usize, y0: usize, x1: usize, y1: usize) -> Self {
        Self { x0, y0, x1, y1 }
    }

    fn intersection(self, other: DirtyRect) -> Option<DirtyRect> {
        let x0 = self.x0.max(other.x0);
        let y0 = self.y0.max(other.y0);
        let x1 = self.x1.min(other.x1);
        let y1 = self.y1.min(other.y1);
        if x1 <= x0 || y1 <= y0 {
            None
        } else {
            Some(DirtyRect::new(x0, y0, x1, y1))
        }
    }

    fn is_empty(self) -> bool {
        self.x1 <= self.x0 || self.y1 <= self.y0
    }

    fn contains(self, other: DirtyRect) -> bool {
        self.x0 <= other.x0 && self.y0 <= other.y0 && self.x1 >= other.x1 && self.y1 >= other.y1
    }

    fn intersects(self, other: DirtyRect) -> bool {
        self.x0 < other.x1 && self.x1 > other.x0 && self.y0 < other.y1 && self.y1 > other.y0
    }

    fn union(self, other: DirtyRect) -> DirtyRect {
        DirtyRect {
            x0: self.x0.min(other.x0),
            y0: self.y0.min(other.y0),
            x1: self.x1.max(other.x1),
            y1: self.y1.max(other.y1),
        }
    }
}

const MAX_DIRTY_RECTS: usize = 8;
const MAX_OCCLUSION_RECTS: usize = 16;

#[derive(Copy, Clone)]
struct DirtyRectList {
    rects: [DirtyRect; MAX_DIRTY_RECTS],
    len: usize,
}

impl DirtyRectList {
    const fn new() -> Self {
        Self {
            rects: [DirtyRect::new(0, 0, 0, 0); MAX_DIRTY_RECTS],
            len: 0,
        }
    }

    fn clear(&mut self) {
        self.len = 0;
    }

    fn add_rect(&mut self, rect: DirtyRect) {
        if rect.is_empty() {
            return;
        }
        for i in 0..self.len {
            if self.rects[i].contains(rect) {
                return;
            }
        }
        let mut merged = rect;
        let mut i = 0;
        while i < self.len {
            if merged.contains(self.rects[i]) || merged.intersects(self.rects[i]) {
                merged = merged.union(self.rects[i]);
                self.rects[i] = self.rects[self.len - 1];
                self.len -= 1;
                i = 0;
                continue;
            }
            i += 1;
        }
        if self.len < MAX_DIRTY_RECTS {
            self.rects[self.len] = merged;
            self.len += 1;
            return;
        }
        let mut collapsed = merged;
        for i in 0..self.len {
            collapsed = collapsed.union(self.rects[i]);
        }
        self.rects[0] = collapsed;
        self.len = 1;
    }

    fn iter(&self) -> core::slice::Iter<'_, DirtyRect> {
        self.rects[..self.len].iter()
    }
}

impl Default for DirtyRectList {
    fn default() -> Self {
        Self::new()
    }
}

fn write_pixel_into_raw(
    buf: &mut [u8],
    stride_px: usize,
    height_px: usize,
    x: usize,
    y: usize,
    color: u32,
    pixel_format: PixelFormat,
    bpp: usize,
) {
    if x >= stride_px || y >= height_px {
        return;
    }
    let off = (y * stride_px + x) * bpp;
    write_pixel_raw_format(buf, off, color, pixel_format, bpp);
}

fn fill_rect_into_raw(
    buf: &mut [u8],
    stride_px: usize,
    height_px: usize,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    color: u32,
    pixel_format: PixelFormat,
    bpp: usize,
) {
    for dy in 0..h {
        let py = y + dy;
        if py >= height_px {
            break;
        }
        for dx in 0..w {
            let px = x + dx;
            if px >= stride_px {
                break;
            }
            let off = (py * stride_px + px) * bpp;
            write_pixel_raw_format(buf, off, color, pixel_format, bpp);
        }
    }
}

const GLYPH_ROW_BUF_MAX: usize = 512;

fn draw_glyph_into_raw(
    dst: &mut [u8],
    dst_stride_px: usize,
    dst_height_px: usize,
    font: &Font,
    scale: usize,
    x_char: usize,
    y_char: usize,
    c: char,
    fg: u32,
    bg: u32,
    pixel_format: PixelFormat,
    bpp: usize,
) {
    let glyph = font.glyph(c);
    let glyph_w_px = font.width.saturating_mul(scale);
    let glyph_h_px = font.height.saturating_mul(scale);
    let base_px = x_char.saturating_mul(glyph_w_px);
    let base_py = y_char.saturating_mul(glyph_h_px);
    if glyph_w_px > 0
        && glyph_h_px > 0
        && base_px.saturating_add(glyph_w_px) <= dst_stride_px
        && base_py.saturating_add(glyph_h_px) <= dst_height_px
        && bpp > 0
    {
        let row_len = glyph_w_px.saturating_mul(bpp);
        if row_len > 0 && row_len <= GLYPH_ROW_BUF_MAX {
            let mut row_buf = [0u8; GLYPH_ROW_BUF_MAX];
            let row_slice = &mut row_buf[..row_len];
            for (row, bits) in glyph.iter().enumerate() {
                let mut x = 0usize;
                for col in 0..font.width {
                    let bit = (bits >> (7 - col)) & 1;
                    let color = if bit == 1 { fg } else { bg };
                    for sx in 0..scale {
                        let off = (x + sx) * bpp;
                        write_pixel_raw_format(row_slice, off, color, pixel_format, bpp);
                    }
                    x += scale;
                }
                let dst_row = base_py + row * scale;
                for dy in 0..scale {
                    let py = dst_row + dy;
                    let dst_off = (py * dst_stride_px + base_px) * bpp;
                    let dst_end = dst_off + row_len;
                    if dst_end <= dst.len() {
                        dst[dst_off..dst_end].copy_from_slice(row_slice);
                    }
                }
            }
            return;
        }
    }
    for (row, bits) in glyph.iter().enumerate() {
        for col in 0..font.width {
            let bit = (bits >> (7 - col)) & 1;
            let pix = if bit == 1 { fg } else { bg };
            let px = base_px + col * scale;
            let py = base_py + row * scale;
            for dy in 0..scale {
                for dx in 0..scale {
                    write_pixel_into_raw(
                        dst,
                        dst_stride_px,
                        dst_height_px,
                        px + dx,
                        py + dy,
                        pix,
                        pixel_format,
                        bpp,
                    );
                }
            }
        }
    }
}

fn blit_image_scaled_into_raw(
    dst: &mut [u8],
    dst_w: usize,
    dst_h: usize,
    dst_stride: usize,
    image: &[u8],
    img_w: usize,
    img_h: usize,
    channels: usize,
    pixel_format: PixelFormat,
    bpp: usize,
) {
    if img_w == 0 || img_h == 0 {
        return;
    }
    if channels != 3 && channels != 4 {
        return;
    }
    if image.len() < img_w.saturating_mul(img_h).saturating_mul(channels) {
        return;
    }
    fill_rect_into_raw(
        dst,
        dst_stride,
        dst_h,
        0,
        0,
        dst_w,
        dst_h,
        0x000000,
        pixel_format,
        bpp,
    );
    let scale_x = dst_w as f32 / img_w as f32;
    let scale_y = dst_h as f32 / img_h as f32;
    let mut scale = scale_x.min(scale_y);
    if scale > 1.0 {
        scale = 1.0;
    }
    if scale <= 0.0 {
        return;
    }
    let target_w = libm::roundf(img_w as f32 * scale) as usize;
    let target_h = libm::roundf(img_h as f32 * scale) as usize;
    if target_w == 0 || target_h == 0 {
        return;
    }
    let offset_x = (dst_w.saturating_sub(target_w)) / 2;
    let offset_y = (dst_h.saturating_sub(target_h)) / 2;
    for ty in 0..target_h {
        let sy = ty * img_h / target_h;
        for tx in 0..target_w {
            let sx = tx * img_w / target_w;
            let src_idx = (sy * img_w + sx) * channels;
            if src_idx + (channels - 1) >= image.len() {
                continue;
            }
            let r = image[src_idx] as u32;
            let g = image[src_idx + 1] as u32;
            let b = image[src_idx + 2] as u32;
            let dst_x = offset_x + tx;
            let dst_y = offset_y + ty;
            if dst_x >= dst_w || dst_y >= dst_h {
                continue;
            }
            let off = (dst_y * dst_stride + dst_x) * bpp;
            write_pixel_raw_format(dst, off, (r << 16) | (g << 8) | b, pixel_format, bpp);
        }
    }
}

fn vga8_glyph(idx: u8) -> &'static [u8] {
    &VGA8_FONT[idx as usize]
}

fn terminus_glyph(idx: u8) -> &'static [u8] {
    &TERMINUS_FONT[idx as usize]
}

fn spleen_glyph(idx: u8) -> &'static [u8] {
    &SPLEEN_FONT[idx as usize]
}

static FONT_VGA8: Font = Font {
    glyph: vga8_glyph,
    width: 8,
    height: 8,
    name: "vga8",
};

static FONT_TERMINUS: Font = Font {
    glyph: terminus_glyph,
    width: 8,
    height: 16,
    name: "terminus",
};

static FONT_SPLEEN: Font = Font {
    glyph: spleen_glyph,
    width: 8,
    height: 16,
    name: "spleen",
};

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum FontKind {
    Vga8,
    Terminus8x16,
    Spleen8x16,
}

impl FontKind {
    fn face(self) -> &'static Font {
        match self {
            FontKind::Vga8 => &FONT_VGA8,
            FontKind::Terminus8x16 => &FONT_TERMINUS,
            FontKind::Spleen8x16 => &FONT_SPLEEN,
        }
    }

    fn default_scale(self) -> usize {
        match self {
            FontKind::Vga8 => 2,
            FontKind::Terminus8x16 => 1,
            FontKind::Spleen8x16 => 1,
        }
    }
}

pub struct Console {
    fb: &'static mut [u8],
    back_buffer: Option<BackBufferStorage>,
    scene_buffer: Option<&'static mut [u8]>,
    info: FrameBufferInfo,
    compositor_mode: CompositorMode,
    layers: [Option<Layer>; MAX_LAYERS],
    hud_layer: Option<LayerId>,
    image_layer: Option<LayerId>,
    font_kind: FontKind,
    width: usize,
    height: usize,
    cursor_x: usize,
    cursor_y: usize,
    scale: usize,
    fg: u32,
    bg: u32,
    reserved_hud_rows: usize,
    dirty: DirtyRectList,
    cursor_style: CursorStyle,
    cursor_blink: CursorBlink,
    cursor_visible: bool,
    cursor_intensity: u8,
    cursor_color: u32,
    blink_timer: u16,
    cursor_saved: Option<CursorSave>,
    clear_row_cache: Option<ClearRowCache>,
    outline_overlay: Option<OutlineOverlay>,
    present_suspended: bool,
    double_buffered: bool,
    classic_mode: bool,
}

pub enum DrawPos {
    Char(usize, usize),
}

pub enum HudAlign {
    Left,
    Center,
    Right,
}

#[derive(Copy, Clone)]
pub enum CursorStyle {
    Underscore,
    Line,
    Block,
    Hidden,
}

#[derive(Copy, Clone)]
pub enum CursorBlink {
    None,
    Pulse,
    Fade,
}

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum CompositorMode {
    Legacy,
    Layered,
}

#[derive(Copy, Clone, PartialEq, Eq)]
pub struct LayerId(u8);

#[derive(Copy, Clone, PartialEq, Eq)]
enum BlendMode {
    Opaque,
    Alpha,
}

enum LayerBuffer {
    Heap(Vec<u8>),
    App {
        app_id: memory::AppId,
        ptr: *mut u8,
        len: usize,
        align: usize,
    },
}

unsafe impl Send for LayerBuffer {}

impl LayerBuffer {
    fn new_heap(len: usize) -> Option<Self> {
        let mut buffer = Vec::new();
        if buffer.try_reserve_exact(len).is_err() {
            return None;
        }
        buffer.resize(len, 0);
        Some(Self::Heap(buffer))
    }

    unsafe fn new_app(app_id: memory::AppId, len: usize, align: usize) -> Option<Self> {
        let ptr = memory::app_alloc(app_id, len, align);
        if ptr.is_null() {
            return None;
        }
        core::ptr::write_bytes(ptr, 0, len);
        Some(Self::App { app_id, ptr, len, align })
    }

    fn len(&self) -> usize {
        match self {
            Self::Heap(buffer) => buffer.len(),
            Self::App { len, .. } => *len,
        }
    }

    fn as_slice(&self) -> &[u8] {
        match self {
            Self::Heap(buffer) => buffer.as_slice(),
            Self::App { ptr, len, .. } => unsafe { core::slice::from_raw_parts(*ptr, *len) },
        }
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        match self {
            Self::Heap(buffer) => buffer.as_mut_slice(),
            Self::App { ptr, len, .. } => unsafe { core::slice::from_raw_parts_mut(*ptr, *len) },
        }
    }
}

impl Drop for LayerBuffer {
    fn drop(&mut self) {
        if let LayerBuffer::App { app_id, ptr, len, align } = self {
            let ptr = *ptr;
            let len = *len;
            let align = *align;
            if !ptr.is_null() && len > 0 {
                unsafe {
                    let _ = memory::app_dealloc(*app_id, ptr, len, align);
                }
            }
        }
    }
}

enum BackBufferStorage {
    Static(&'static mut [u8]),
    Heap(Vec<u8>),
}

impl BackBufferStorage {
    fn len(&self) -> usize {
        match self {
            Self::Static(buffer) => buffer.len(),
            Self::Heap(buffer) => buffer.len(),
        }
    }

    fn as_slice(&self) -> &[u8] {
        match self {
            Self::Static(buffer) => *buffer,
            Self::Heap(buffer) => buffer.as_slice(),
        }
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        match self {
            Self::Static(buffer) => *buffer,
            Self::Heap(buffer) => buffer.as_mut_slice(),
        }
    }
}

struct Layer {
    buffer: LayerBuffer,
    width: usize,
    height: usize,
    stride: usize,
    x: usize,
    y: usize,
    alpha: u8,
    visible: bool,
    z: i16,
    blend: BlendMode,
    dirty: Option<(usize, usize, usize, usize)>,
}

impl LayerId {
    fn idx(self) -> usize {
        self.0 as usize
    }
}

const MAX_BACKBUFFER_BYTES: usize = 32 * 1024 * 1024;
static mut BACK_BUFFER_STORAGE: MaybeUninit<[u8; MAX_BACKBUFFER_BYTES]> = MaybeUninit::uninit();
static mut SCENE_BUFFER_STORAGE: MaybeUninit<[u8; MAX_BACKBUFFER_BYTES]> = MaybeUninit::uninit();
const CURSOR_SNAPSHOT_MAX: usize = 8192;

struct CursorSave {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    len: usize,
    data: [u8; CURSOR_SNAPSHOT_MAX],
}

struct ClearRowCache {
    color: u32,
    width_px: usize,
    bytes_per_pixel: usize,
    pixel_format: PixelFormat,
    row: Vec<u8>,
}

#[derive(Copy, Clone)]
struct OutlineOverlay {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    color: u32,
    thickness: usize,
}

fn alloc_back_buffer(len: usize) -> Option<BackBufferStorage> {
    if len > MAX_BACKBUFFER_BYTES {
        return None;
    }
    let mut buffer = Vec::new();
    if buffer.try_reserve_exact(len).is_ok() {
        buffer.resize(len, 0);
        return Some(BackBufferStorage::Heap(buffer));
    }
    unsafe {
        let ptr = addr_of_mut!(BACK_BUFFER_STORAGE) as *mut u8;
        Some(BackBufferStorage::Static(core::slice::from_raw_parts_mut(ptr, len)))
    }
}

fn alloc_scene_buffer(len: usize) -> Option<&'static mut [u8]> {
    if len > MAX_BACKBUFFER_BYTES {
        return None;
    }
    unsafe {
        let ptr = addr_of_mut!(SCENE_BUFFER_STORAGE) as *mut u8;
        Some(core::slice::from_raw_parts_mut(ptr, len))
    }
}

const MAX_LAYERS: usize = 8;
const HUD_LAYER_Z: i16 = 100;
const IMAGE_LAYER_Z: i16 = 1000;

#[derive(Copy, Clone)]
pub struct DisplayBufferStats {
    pub framebuffer_bytes: usize,
    pub backbuffer_bytes: usize,
    pub width_px: usize,
    pub height_px: usize,
    pub stride_px: usize,
    pub bytes_per_pixel: usize,
}

impl Console {
    fn font(&self) -> &'static Font {
        self.font_kind.face()
    }

    fn char_w(&self) -> usize {
        self.font().width * self.scale
    }

    fn char_h(&self) -> usize {
        self.font().height * self.scale
    }

    fn base_buffer(&self) -> &[u8] {
        match self.compositor_mode {
            CompositorMode::Legacy => {
                if self.double_buffered {
                    self.back_buffer
                        .as_ref()
                        .map(|buf| buf.as_slice())
                        .unwrap_or(&*self.fb)
                } else {
                    &*self.fb
                }
            }
            CompositorMode::Layered => {
                if let Some(scene) = self.scene_buffer.as_deref() {
                    scene
                } else if self.double_buffered {
                    self.back_buffer
                        .as_ref()
                        .map(|buf| buf.as_slice())
                        .unwrap_or(&*self.fb)
                } else {
                    &*self.fb
                }
            }
        }
    }

    fn base_buffer_mut(&mut self) -> &mut [u8] {
        match self.compositor_mode {
            CompositorMode::Legacy => {
                if self.double_buffered {
                    self.back_buffer
                        .as_mut()
                        .map(|buf| buf.as_mut_slice())
                        .unwrap_or(&mut *self.fb)
                } else {
                    &mut *self.fb
                }
            }
            CompositorMode::Layered => {
                if let Some(scene) = self.scene_buffer.as_deref_mut() {
                    scene
                } else if self.double_buffered {
                    self.back_buffer
                        .as_mut()
                        .map(|buf| buf.as_mut_slice())
                        .unwrap_or(&mut *self.fb)
                } else {
                    &mut *self.fb
                }
            }
        }
    }

    fn recompute_dimensions(&mut self) {
        self.width = self.info.width / self.char_w();
        self.height = self.info.height / self.char_h();

        let reserved = self.reserved_hud_rows;
        if reserved > 0 {
            self.reserve_hud_rows(reserved);
        }

        let text_h = self.text_area_height();
        if self.width > 0 {
            self.cursor_x = self.cursor_x.min(self.width - 1);
        } else {
            self.cursor_x = 0;
        }
        if text_h > 0 {
            self.cursor_y = self.cursor_y.min(text_h - 1);
        } else {
            self.cursor_y = 0;
        }
    }

    fn mark_dirty(&mut self, x: usize, y: usize, w: usize, h: usize) {
        if w == 0 || h == 0 {
            return;
        }
        let max_x = self.info.width;
        let max_y = self.info.height;
        if x >= max_x || y >= max_y {
            return;
        }
        let x1 = (x + w).min(max_x);
        let y1 = (y + h).min(max_y);
        if x1 <= x || y1 <= y {
            return;
        }
        self.dirty.add_rect(DirtyRect::new(x, y, x1, y1));
    }

    fn set_outline_overlay(&mut self, overlay: Option<OutlineOverlay>) {
        if let Some(prev) = self.outline_overlay {
            self.mark_dirty(prev.x, prev.y, prev.w, prev.h);
        }
        self.outline_overlay = overlay;
        if let Some(next) = self.outline_overlay {
            self.mark_dirty(next.x, next.y, next.w, next.h);
        }
    }

    fn draw_outline_overlay(&mut self, x: usize, y: usize, w: usize, h: usize) {
        let Some(overlay) = self.outline_overlay else {
            return;
        };
        if w == 0 || h == 0 || overlay.w == 0 || overlay.h == 0 {
            return;
        }
        let thickness = overlay.thickness.min(overlay.w).min(overlay.h);
        if thickness == 0 {
            return;
        }
        let rx0 = x;
        let ry0 = y;
        let rx1 = x.saturating_add(w);
        let ry1 = y.saturating_add(h);
        let ox0 = overlay.x;
        let oy0 = overlay.y;
        let ox1 = overlay.x.saturating_add(overlay.w);
        let oy1 = overlay.y.saturating_add(overlay.h);
        if rx1 <= ox0 || ry1 <= oy0 || rx0 >= ox1 || ry0 >= oy1 {
            return;
        }
        let target = if self.double_buffered {
            match self.back_buffer.as_mut() {
                Some(buffer) => buffer.as_mut_slice(),
                None => &mut *self.fb,
            }
        } else {
            &mut *self.fb
        };
        let stride = self.info.stride;
        let height = self.info.height;
        let bpp = self.info.bytes_per_pixel;
        let pixel_format = self.info.pixel_format;
        let right_x = overlay.x.saturating_add(overlay.w.saturating_sub(thickness));
        let bottom_y = overlay.y.saturating_add(overlay.h.saturating_sub(thickness));
        let edges = [
            (overlay.x, overlay.y, overlay.w, thickness),
            (overlay.x, bottom_y, overlay.w, thickness),
            (overlay.x, overlay.y, thickness, overlay.h),
            (right_x, overlay.y, thickness, overlay.h),
        ];
        for (ex, ey, ew, eh) in edges {
            let ex1 = ex.saturating_add(ew);
            let ey1 = ey.saturating_add(eh);
            if ex1 <= rx0 || ey1 <= ry0 || ex >= rx1 || ey >= ry1 {
                continue;
            }
            let ix0 = ex.max(rx0);
            let iy0 = ey.max(ry0);
            let ix1 = ex1.min(rx1);
            let iy1 = ey1.min(ry1);
            if ix1 <= ix0 || iy1 <= iy0 {
                continue;
            }
            fill_rect_into_raw(
                target,
                stride,
                height,
                ix0,
                iy0,
                ix1 - ix0,
                iy1 - iy0,
                overlay.color,
                pixel_format,
                bpp,
            );
        }
    }

    fn present(&mut self) {
        if self.present_suspended {
            return;
        }
        self.flush_layer_dirty();
        let dirty = core::mem::take(&mut self.dirty);
        for rect in dirty.iter() {
            self.present_rect(rect.x0, rect.y0, rect.x1 - rect.x0, rect.y1 - rect.y0);
        }
    }

    fn present_rect(&mut self, x: usize, y: usize, w: usize, h: usize) {
        if self.present_suspended {
            return;
        }
        if w == 0 || h == 0 {
            return;
        }
        let max_x = self.info.width;
        let max_y = self.info.height;
        if x >= max_x || y >= max_y {
            return;
        }
        let x1 = (x + w).min(max_x);
        let y1 = (y + h).min(max_y);
        self.composite_rect(x, y, x1 - x, y1 - y);
        self.draw_outline_overlay(x, y, x1 - x, y1 - y);
        if !self.double_buffered {
            return;
        }
        let Some(back_buffer) = self.back_buffer.as_ref() else {
            return;
        };
        let src = back_buffer.as_slice();
        let bpp = self.info.bytes_per_pixel;
        let stride = self.info.stride;
        for row in y..y1 {
            let off = (row * stride + x) * bpp;
            let len = (x1 - x) * bpp;
            let src = &src[off..off + len];
            let dst = &mut self.fb[off..off + len];
            dst.copy_from_slice(src);
        }
    }

    fn present_full(&mut self) {
        if self.present_suspended {
            return;
        }
        self.clear_layer_dirty();
        self.present_rect(0, 0, self.info.width, self.info.height);
        self.dirty.clear();
    }

    fn copy_scene_rect(&mut self, x: usize, y: usize, w: usize, h: usize) {
        let Some(scene) = self.scene_buffer.as_deref() else {
            return;
        };
        let target = if self.double_buffered {
            match self.back_buffer.as_mut() {
                Some(buffer) => buffer.as_mut_slice(),
                None => &mut *self.fb,
            }
        } else {
            &mut *self.fb
        };
        let max_x = self.info.width;
        let max_y = self.info.height;
        if x >= max_x || y >= max_y {
            return;
        }
        let x1 = (x + w).min(max_x);
        let y1 = (y + h).min(max_y);
        let bpp = self.info.bytes_per_pixel;
        let stride = self.info.stride;
        for row in y..y1 {
            let off = (row * stride + x) * bpp;
            let len = (x1 - x) * bpp;
            if off + len > scene.len() || off + len > target.len() {
                break;
            }
            let src = &scene[off..off + len];
            let dst = &mut target[off..off + len];
            dst.copy_from_slice(src);
        }
    }

    fn collect_layer_order(&self, order: &mut [usize; MAX_LAYERS]) -> usize {
        let mut count = 0;
        for (idx, layer) in self.layers.iter().enumerate() {
            if let Some(layer) = layer {
                if layer.visible {
                    order[count] = idx;
                    count += 1;
                }
            }
        }
        if count > 1 {
            order[..count].sort_unstable_by_key(|idx| {
                let z = self.layers[*idx].as_ref().map(|l| l.z).unwrap_or(0);
                (z, *idx)
            });
        }
        count
    }

    fn blend_color_precalc(dst: u32, src: u32, alpha: u32, inv: u32) -> u32 {
        let sr = ((src >> 16) & 0xFF) as u32;
        let sg = ((src >> 8) & 0xFF) as u32;
        let sb = (src & 0xFF) as u32;
        let dr = ((dst >> 16) & 0xFF) as u32;
        let dg = ((dst >> 8) & 0xFF) as u32;
        let db = (dst & 0xFF) as u32;
        let r = (sr * alpha + dr * inv) / 255;
        let g = (sg * alpha + dg * inv) / 255;
        let b = (sb * alpha + db * inv) / 255;
        (r << 16) | (g << 8) | b
    }

    fn blend_layer_rect(&mut self, layer_idx: usize, x: usize, y: usize, w: usize, h: usize) {
        let max_x = self.info.width;
        let max_y = self.info.height;
        if x >= max_x || y >= max_y {
            return;
        }
        let bpp = self.info.bytes_per_pixel;
        let dst_stride = self.info.stride;
        let pixel_format = self.info.pixel_format;
        let layers = &self.layers;
        let target = if self.double_buffered {
            match self.back_buffer.as_mut() {
                Some(buffer) => buffer.as_mut_slice(),
                None => &mut *self.fb,
            }
        } else {
            &mut *self.fb
        };
        let Some(layer) = layers.get(layer_idx).and_then(|l| l.as_ref()) else {
            return;
        };
        if !layer.visible {
            return;
        }
        let x1 = (x + w).min(max_x);
        let y1 = (y + h).min(max_y);
        let layer_x0 = layer.x;
        let layer_y0 = layer.y;
        let layer_x1 = layer_x0.saturating_add(layer.width);
        let layer_y1 = layer_y0.saturating_add(layer.height);
        let ix0 = x.max(layer_x0);
        let iy0 = y.max(layer_y0);
        let ix1 = x1.min(layer_x1);
        let iy1 = y1.min(layer_y1);
        if ix1 <= ix0 || iy1 <= iy0 {
            return;
        }
        let src_stride = layer.stride;
        let layer_buf = layer.buffer.as_slice();
        let row_px = ix1 - ix0;
        let row_len = row_px * bpp;
        let src_x_off = (ix0 - layer_x0) * bpp;
        let dst_x_off = ix0 * bpp;
        let fast_4bpp = bpp == 4 && matches!(pixel_format, PixelFormat::Rgb | PixelFormat::Bgr);
        let fast_copy = matches!(pixel_format, PixelFormat::Rgb | PixelFormat::Bgr);
        match layer.blend {
            BlendMode::Opaque => {
                for py in iy0..iy1 {
                    let src_row = (py - layer_y0) * src_stride;
                    let dst_row = py * dst_stride;
                    let src_off = src_row * bpp + src_x_off;
                    let dst_off = dst_row * bpp + dst_x_off;
                    if row_len > 0
                        && src_off + row_len <= layer_buf.len()
                        && dst_off + row_len <= target.len()
                    {
                        let src = &layer_buf[src_off..src_off + row_len];
                        let dst = &mut target[dst_off..dst_off + row_len];
                        if fast_copy {
                            dst.copy_from_slice(src);
                            continue;
                        }
                    }
                    for px in ix0..ix1 {
                        let src_off = (src_row + (px - layer_x0)) * bpp;
                        let dst_off = (dst_row + px) * bpp;
                        if src_off + bpp > layer_buf.len() || dst_off + bpp > target.len() {
                            continue;
                        }
                        let src_color = read_pixel_raw_format(layer_buf, src_off, pixel_format, bpp);
                        write_pixel_raw_format(target, dst_off, src_color, pixel_format, bpp);
                    }
                }
            }
            BlendMode::Alpha => {
                if layer.alpha == 0 {
                    return;
                }
                if layer.alpha >= 255 {
                    for py in iy0..iy1 {
                        let src_row = (py - layer_y0) * src_stride;
                        let dst_row = py * dst_stride;
                        let src_off = src_row * bpp + src_x_off;
                        let dst_off = dst_row * bpp + dst_x_off;
                        if row_len > 0
                            && src_off + row_len <= layer_buf.len()
                            && dst_off + row_len <= target.len()
                        {
                            let src = &layer_buf[src_off..src_off + row_len];
                            let dst = &mut target[dst_off..dst_off + row_len];
                            if fast_copy {
                                dst.copy_from_slice(src);
                                continue;
                            }
                        }
                        for px in ix0..ix1 {
                            let src_off = (src_row + (px - layer_x0)) * bpp;
                            let dst_off = (dst_row + px) * bpp;
                            if src_off + bpp > layer_buf.len() || dst_off + bpp > target.len() {
                                continue;
                            }
                            let src_color = read_pixel_raw_format(layer_buf, src_off, pixel_format, bpp);
                            write_pixel_raw_format(target, dst_off, src_color, pixel_format, bpp);
                        }
                    }
                    return;
                }
                let alpha = layer.alpha as u32;
                let inv = 255 - alpha;
                for py in iy0..iy1 {
                    let src_row = (py - layer_y0) * src_stride;
                    let dst_row = py * dst_stride;
                    let src_off = src_row * bpp + src_x_off;
                    let dst_off = dst_row * bpp + dst_x_off;
                    if fast_4bpp
                        && row_len > 0
                        && src_off + row_len <= layer_buf.len()
                        && dst_off + row_len <= target.len()
                    {
                        let src = &layer_buf[src_off..src_off + row_len];
                        let dst = &mut target[dst_off..dst_off + row_len];
                        blend_row_alpha_4bpp(dst, src, layer.alpha);
                        continue;
                    }
                    for px in ix0..ix1 {
                        let src_off = (src_row + (px - layer_x0)) * bpp;
                        let dst_off = (dst_row + px) * bpp;
                        if src_off + bpp > layer_buf.len() || dst_off + bpp > target.len() {
                            continue;
                        }
                        let src_color = read_pixel_raw_format(layer_buf, src_off, pixel_format, bpp);
                        let dst_color = read_pixel_raw_format(target, dst_off, pixel_format, bpp);
                        let out_color = Self::blend_color_precalc(dst_color, src_color, alpha, inv);
                        write_pixel_raw_format(target, dst_off, out_color, pixel_format, bpp);
                    }
                }
            }
        }
    }

    fn blend_layers_rect(&mut self, order: &[usize], start: usize, rect: DirtyRect, skip_scene: bool) {
        if rect.is_empty() || start >= order.len() {
            return;
        }
        let w = rect.x1 - rect.x0;
        let h = rect.y1 - rect.y0;
        if w == 0 || h == 0 {
            return;
        }
        if !skip_scene {
            self.copy_scene_rect(rect.x0, rect.y0, w, h);
        }
        for idx in order[start..].iter().copied() {
            self.blend_layer_rect(idx, rect.x0, rect.y0, w, h);
        }
    }

    fn find_topmost_opaque_intersection(
        &self,
        order: &[usize],
        rect: DirtyRect,
    ) -> Option<(usize, DirtyRect)> {
        for (pos, idx) in order.iter().enumerate().rev() {
            let Some(layer) = self.layers[*idx].as_ref() else {
                continue;
            };
            if !layer.visible {
                continue;
            }
            let is_opaque = matches!(layer.blend, BlendMode::Opaque) || layer.alpha >= 255;
            if !is_opaque {
                continue;
            }
            let layer_rect = DirtyRect::new(
                layer.x,
                layer.y,
                layer.x.saturating_add(layer.width),
                layer.y.saturating_add(layer.height),
            );
            if let Some(intersection) = rect.intersection(layer_rect) {
                return Some((pos, intersection));
            }
        }
        None
    }

    fn composite_rect_with_occlusion(&mut self, order: &[usize], rect: DirtyRect) {
        if rect.is_empty() {
            return;
        }
        let mut stack = [DirtyRect::new(0, 0, 0, 0); MAX_OCCLUSION_RECTS];
        let mut stack_len = 0usize;
        stack[stack_len] = rect;
        stack_len += 1;

        while stack_len > 0 {
            stack_len -= 1;
            let cur = stack[stack_len];
            if cur.is_empty() {
                continue;
            }

            let Some((pos, intersection)) = self.find_topmost_opaque_intersection(order, cur) else {
                self.blend_layers_rect(order, 0, cur, false);
                continue;
            };

            if intersection.contains(cur) {
                self.blend_layers_rect(order, pos, cur, true);
                continue;
            }

            let left = cur.x0 < intersection.x0;
            let right = intersection.x1 < cur.x1;
            let top = cur.y0 < intersection.y0;
            let bottom = intersection.y1 < cur.y1;
            let needed = left as usize + right as usize + top as usize + bottom as usize;
            if stack_len + needed > MAX_OCCLUSION_RECTS {
                self.blend_layers_rect(order, 0, cur, false);
                continue;
            }

            self.blend_layers_rect(order, pos, intersection, true);

            if left {
                stack[stack_len] = DirtyRect::new(cur.x0, cur.y0, intersection.x0, cur.y1);
                stack_len += 1;
            }
            if right {
                stack[stack_len] = DirtyRect::new(intersection.x1, cur.y0, cur.x1, cur.y1);
                stack_len += 1;
            }
            if top {
                stack[stack_len] = DirtyRect::new(intersection.x0, cur.y0, intersection.x1, intersection.y0);
                stack_len += 1;
            }
            if bottom {
                stack[stack_len] = DirtyRect::new(intersection.x0, intersection.y1, intersection.x1, cur.y1);
                stack_len += 1;
            }
        }
    }

    fn composite_rect(&mut self, x: usize, y: usize, w: usize, h: usize) {
        if self.compositor_mode != CompositorMode::Layered {
            return;
        }
        if self.scene_buffer.is_none() {
            return;
        }
        if w == 0 || h == 0 {
            return;
        }
        let mut order = [0usize; MAX_LAYERS];
        let count = self.collect_layer_order(&mut order);
        if count == 0 {
            self.copy_scene_rect(x, y, w, h);
            return;
        }
        let rect = DirtyRect::new(x, y, x.saturating_add(w), y.saturating_add(h));
        self.composite_rect_with_occlusion(&order[..count], rect);
    }

    fn flush_layer_dirty(&mut self) {
        if self.compositor_mode != CompositorMode::Layered {
            return;
        }
        for idx in 0..self.layers.len() {
            let (visible, layer_x, layer_y, dirty) = {
                let Some(layer) = self.layers[idx].as_mut() else {
                    continue;
                };
                let dirty = layer.dirty.take();
                (layer.visible, layer.x, layer.y, dirty)
            };
            if let Some((x0, y0, x1, y1)) = dirty {
                if visible {
                    let w = x1 - x0;
                    let h = y1 - y0;
                    self.mark_dirty(layer_x.saturating_add(x0), layer_y.saturating_add(y0), w, h);
                } else if let Some(layer) = self.layers[idx].as_mut() {
                    layer.dirty = Some((x0, y0, x1, y1));
                }
            }
        }
    }

    fn clear_layer_dirty(&mut self) {
        if self.compositor_mode != CompositorMode::Layered {
            return;
        }
        for layer in self.layers.iter_mut().flatten() {
            layer.dirty = None;
        }
    }

    fn buffer_stats(&self) -> DisplayBufferStats {
        let backbuffer_bytes = if self.double_buffered {
            self.back_buffer.as_ref().map(|buf| buf.len()).unwrap_or(0)
        } else {
            0
        };
        DisplayBufferStats {
            framebuffer_bytes: self.fb.len(),
            backbuffer_bytes,
            width_px: self.info.width,
            height_px: self.info.height,
            stride_px: self.info.stride,
            bytes_per_pixel: self.info.bytes_per_pixel,
        }
    }

    pub fn from_boot_info(boot: &'static mut BootInfo) -> Option<Self> {
        let fb = boot.framebuffer.as_mut()?;
        let info = fb.info();
        let slice = fb.buffer_mut();
        let mut back_buffer = alloc_back_buffer(slice.len());
        if let Some(back_buffer) = back_buffer.as_mut() {
            let dst = back_buffer.as_mut_slice();
            let len = core::cmp::min(dst.len(), slice.len());
            dst[..len].copy_from_slice(&slice[..len]);
        }
        let mut scene_buffer = alloc_scene_buffer(slice.len());
        if let Some(scene) = scene_buffer.as_deref_mut() {
            scene.copy_from_slice(slice);
        }
        let compositor_mode = if scene_buffer.is_some() {
            CompositorMode::Layered
        } else {
            CompositorMode::Legacy
        };
        let double_buffered = back_buffer.is_some();
        let layers: [Option<Layer>; MAX_LAYERS] = [(); MAX_LAYERS].map(|_| None);
        let font_kind = FontKind::Vga8;
        let scale = font_kind.default_scale();
        let font = font_kind.face();
        let width = info.width / (font.width * scale);
        let height = info.height / (font.height * scale);
        Some(Self {
            fb: slice,
            back_buffer,
            scene_buffer,
            info,
            compositor_mode,
            layers,
            hud_layer: None,
            image_layer: None,
            font_kind,
            width,
            height,
            cursor_x: 0,
            cursor_y: 0,
            scale,
            fg: 0xCCCCCC,
            bg: 0x000000,
            reserved_hud_rows: 0,
            dirty: DirtyRectList::new(),
            cursor_style: CursorStyle::Line,
            cursor_blink: CursorBlink::Pulse,
            cursor_visible: true,
            cursor_intensity: 255,
            cursor_color: 0xFFFFFF,
            blink_timer: 0,
            cursor_saved: None,
            clear_row_cache: None,
            outline_overlay: None,
            present_suspended: false,
            double_buffered,
            classic_mode: false,
        })
    }

    fn write_pixel_raw(&self, buf: &mut [u8], off: usize, color: u32) {
        write_pixel_raw_format(buf, off, color, self.info.pixel_format, self.info.bytes_per_pixel);
    }

    fn read_pixel_raw(&self, buf: &[u8], off: usize) -> u32 {
        read_pixel_raw_format(buf, off, self.info.pixel_format, self.info.bytes_per_pixel)
    }

    fn write_pixel_to_back(&mut self, off: usize, color: u32) {
        let pixel_format = self.info.pixel_format;
        let bpp = self.info.bytes_per_pixel;
        let buf = self.base_buffer_mut();
        write_pixel_raw_format(buf, off, color, pixel_format, bpp);
    }

    pub fn framebuffer_info(&self) -> &FrameBufferInfo {
        &self.info
    }

    fn write_pixel_into(
        &self,
        buf: &mut [u8],
        buf_stride_px: usize,
        buf_height_px: usize,
        x: usize,
        y: usize,
        color: u32,
    ) {
        if x >= buf_stride_px || y >= buf_height_px {
            return;
        }
        write_pixel_into_raw(
            buf,
            buf_stride_px,
            buf_height_px,
            x,
            y,
            color,
            self.info.pixel_format,
            self.info.bytes_per_pixel,
        );
    }

    fn draw_glyph_into(
        &self,
        dst: &mut [u8],
        dst_stride_px: usize,
        dst_height_px: usize,
        x_char: usize,
        y_char: usize,
        c: char,
        fg: u32,
        bg: u32,
    ) {
        draw_glyph_into_raw(
            dst,
            dst_stride_px,
            dst_height_px,
            self.font(),
            self.scale,
            x_char,
            y_char,
            c,
            fg,
            bg,
            self.info.pixel_format,
            self.info.bytes_per_pixel,
        );
    }

    fn fill_rect_into(
        &self,
        dst: &mut [u8],
        dst_stride_px: usize,
        dst_height_px: usize,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        color: u32,
    ) {
        fill_rect_into_raw(
            dst,
            dst_stride_px,
            dst_height_px,
            x,
            y,
            w,
            h,
            color,
            self.info.pixel_format,
            self.info.bytes_per_pixel,
        );
    }

    fn draw_glyph(&mut self, x: usize, y: usize, c: char, color: u32) {
        let font = self.font();
        let scale = self.scale;
        let base_px = x.saturating_mul(font.width.saturating_mul(scale));
        let base_py = y.saturating_mul(font.height.saturating_mul(scale));
        self.mark_dirty(base_px, base_py, font.width * scale, font.height * scale);
        let stride = self.info.stride;
        let height = self.info.height;
        let pixel_format = self.info.pixel_format;
        let bpp = self.info.bytes_per_pixel;
        let bg = self.bg;
        let buffer = self.base_buffer_mut();
        draw_glyph_into_raw(
            buffer,
            stride,
            height,
            font,
            scale,
            x,
            y,
            c,
            color,
            bg,
            pixel_format,
            bpp,
        );
    }

    fn fill_rect(&mut self, x: usize, y: usize, w: usize, h: usize, color: u32) {
        self.mark_dirty(x, y, w, h);
        self.fill_rect_raw(x, y, w, h, color);
    }

    fn fill_rect_raw(&mut self, x: usize, y: usize, w: usize, h: usize, color: u32) {
        let bytes_per_pixel = self.info.bytes_per_pixel;
        let stride = self.info.stride;
        for dy in 0..h {
            let py = y + dy;
            if py >= self.info.height {
                break;
            }
            for dx in 0..w {
                let px = x + dx;
                if px >= self.info.width {
                    break;
                }
                let off = (py * stride + px) * bytes_per_pixel;
                self.write_pixel_to_back(off, color);
            }
        }
    }

    fn prepare_clear_row_cache(&mut self, color: u32) -> Option<(*const u8, usize)> {
        let width_px = self.info.width;
        let bpp = self.info.bytes_per_pixel;
        if width_px == 0 || bpp == 0 {
            return None;
        }
        let row_len = width_px.checked_mul(bpp)?;
        if row_len == 0 {
            return None;
        }
        let pixel_format = self.info.pixel_format;
        let needs_new = match self.clear_row_cache.as_ref() {
            Some(cache) => {
                cache.color != color
                    || cache.width_px != width_px
                    || cache.bytes_per_pixel != bpp
                    || cache.pixel_format != pixel_format
            }
            None => true,
        };
        if needs_new {
            let mut row = Vec::with_capacity(row_len);
            row.resize(row_len, 0);
            let mut off = 0;
            while off < row_len {
                write_pixel_raw_format(&mut row, off, color, pixel_format, bpp);
                off += bpp;
            }
            self.clear_row_cache = Some(ClearRowCache {
                color,
                width_px,
                bytes_per_pixel: bpp,
                pixel_format,
                row,
            });
        }
        let cache = self.clear_row_cache.as_ref()?;
        Some((cache.row.as_ptr(), cache.row.len()))
    }

    fn fill_full_width_rows_fast(&mut self, y: usize, h: usize, color: u32) {
        let width_px = self.info.width;
        if self.classic_mode {
            self.fill_rect_raw(0, y, width_px, h, color);
            return;
        }
        let bpp = self.info.bytes_per_pixel;
        let stride = self.info.stride;
        let max_y = self.info.height;
        let Some((row_ptr, row_len)) = self.prepare_clear_row_cache(color) else {
            self.fill_rect_raw(0, y, width_px, h, color);
            return;
        };
        let y1 = (y + h).min(max_y);
        let buffer = self.base_buffer_mut();
        for py in y..y1 {
            let off = (py * stride) * bpp;
            if off + row_len > buffer.len() {
                break;
            }
            unsafe {
                core::ptr::copy_nonoverlapping(row_ptr, buffer.as_mut_ptr().add(off), row_len);
            }
        }
    }

    pub fn clear(&mut self) {
        self.erase_cursor();
        self.mark_dirty(0, 0, self.info.width, self.info.height);
        self.fill_full_width_rows_fast(0, self.info.height, self.bg);
        if self.compositor_mode == CompositorMode::Layered {
            if let Some(id) = self.hud_layer {
                self.layer_clear(id, self.bg);
            }
        }
        self.cursor_x = 0;
        self.cursor_y = 0;
        self.cursor_visible = true;
        self.cursor_intensity = 255;
        self.draw_cursor();
        self.present();
    }

    pub fn clear_text_area(&mut self) {
        self.erase_cursor();
        let text_h_px = self.text_area_height().saturating_mul(self.char_h());
        if text_h_px > 0 {
            self.mark_dirty(0, 0, self.info.width, text_h_px);
            self.fill_full_width_rows_fast(0, text_h_px, self.bg);
        }
        self.cursor_x = 0;
        self.cursor_y = 0;
        self.cursor_visible = true;
        self.cursor_intensity = 255;
        self.draw_cursor();
        self.present();
    }

    pub fn put_char(&mut self, c: char) {
        if c == '\n' {
            self.newline();
            return;
        }
        self.erase_cursor();
        self.draw_glyph(self.cursor_x, self.cursor_y, c, self.fg);
        self.cursor_x += 1;
        if self.cursor_x >= self.width {
            self.newline();
            return;
        }
        self.cursor_visible = true;
        self.cursor_intensity = 255;
        self.draw_cursor();
        self.present();
    }

    pub fn write_line(&mut self, s: &str) {
        for c in s.chars() {
            self.put_char(c);
        }
        self.put_char('\n');
    }

    pub fn write_inline(&mut self, s: &str) {
        self.erase_cursor();
        let max_y = self.text_area_height();
        if self.info.width == 0 || max_y == 0 {
            return;
        }
        if self.cursor_y >= max_y {
            self.cursor_y = max_y - 1;
        }
        let line_px = self.cursor_y.saturating_mul(self.char_h());
        self.fill_rect(0, line_px, self.info.width, self.char_h(), self.bg);
        self.cursor_x = 0;
        for c in s.chars() {
            self.put_char(c);
        }
        self.cursor_visible = true;
        self.cursor_intensity = 255;
        self.draw_cursor();
        self.present();
    }

    pub fn write(&mut self, s: &str) {
        for c in s.chars() {
            self.put_char(c);
        }
    }

    pub fn newline(&mut self) {
        self.erase_cursor();
        self.cursor_x = 0;
        self.cursor_y += 1;
        if self.cursor_y >= self.text_area_height() {
            self.scroll();
            self.cursor_y = self.text_area_height() - 1;
        }
        self.cursor_visible = true;
        self.cursor_intensity = 255;
        self.draw_cursor();
        self.present();
    }

    pub fn backspace(&mut self) {
        if self.width == 0 {
            return;
        }

        if self.cursor_x == 0 {
            if self.cursor_y == 0 {
                return;
            }
            self.erase_cursor();
            self.cursor_y -= 1;
            self.cursor_x = self.width - 1;
        } else {
            self.erase_cursor();
            self.cursor_x -= 1;
        }

        self.draw_glyph(self.cursor_x, self.cursor_y, ' ', self.bg);
        self.cursor_visible = true;
        self.cursor_intensity = 255;
        self.draw_cursor();
        self.present();
    }

    fn scroll(&mut self) {
        let char_h_px = self.char_h();
        let visible_rows = self.text_area_height();
        if visible_rows == 0 {
            return;
        }

        let visible_px = visible_rows * char_h_px;
        let bpp = self.info.bytes_per_pixel;
        let stride = self.info.stride;
        let shift = char_h_px * stride * bpp;
        let copy_bytes = visible_px * stride * bpp;

        if copy_bytes <= shift {
            return;
        }

        let buffer = self.base_buffer_mut();
        buffer.copy_within(shift..copy_bytes, 0);
        self.mark_dirty(0, 0, self.info.width, visible_px);
        let clear_py = visible_px.saturating_sub(char_h_px);
        self.fill_rect(0, clear_py, self.info.width, char_h_px, self.bg);
    }

    fn apply_intensity(color: u32, base: u32, intensity: u8) -> u32 {
        if intensity >= 255 {
            return color;
        }
        if intensity == 0 {
            return base;
        }
        let r = ((color >> 16) & 0xFF) as u8;
        let g = ((color >> 8) & 0xFF) as u8;
        let b = (color & 0xFF) as u8;
        let br = ((base >> 16) & 0xFF) as u8;
        let bg = ((base >> 8) & 0xFF) as u8;
        let bb = (base & 0xFF) as u8;
        let scale = intensity as u32;
        let r2 = (br as u32 + ((r as u32).saturating_sub(br as u32)) * scale / 255) as u8;
        let g2 = (bg as u32 + ((g as u32).saturating_sub(bg as u32)) * scale / 255) as u8;
        let b2 = (bb as u32 + ((b as u32).saturating_sub(bb as u32)) * scale / 255) as u8;
        ((r2 as u32) << 16) | ((g2 as u32) << 8) | (b2 as u32)
    }

    fn cursor_rect(&self) -> Option<(usize, usize, usize, usize)> {
        let s = self.scale;
        let font = self.font();
        let px = self.cursor_x * font.width * s;
        let py = self.cursor_y * font.height * s;
        match self.cursor_style {
            CursorStyle::Underscore => Some((px, py + (font.height - 1) * s, font.width * s, s)),
            CursorStyle::Line => Some((px, py, 2 * s, font.height * s)),
            CursorStyle::Block => Some((px, py, font.width * s, font.height * s)),
            CursorStyle::Hidden => None,
        }
    }

    fn save_cursor_area(&mut self, x: usize, y: usize, w: usize, h: usize) {
        let bpp = self.info.bytes_per_pixel;
        let stride = self.info.stride;
        let max_w = self.info.width.saturating_sub(x);
        let copy_w = w.min(max_w);
        if copy_w == 0 || h == 0 {
            self.cursor_saved = None;
            return;
        }
        let max_bytes = copy_w
            .saturating_mul(h)
            .saturating_mul(bpp);
        if max_bytes == 0 || max_bytes > CURSOR_SNAPSHOT_MAX {
            self.cursor_saved = None;
            return;
        }
        let mut snap = CursorSave { x, y, w: copy_w, h, len: 0, data: [0; CURSOR_SNAPSHOT_MAX] };
        let buffer = self.base_buffer();
        for row in 0..h {
            let py = y + row;
            if py >= self.info.height {
                break;
            }
            let off = (py * stride + x) * bpp;
            let row_bytes = copy_w * bpp;
            let src_end = off + row_bytes;
            let dst_end = snap.len + row_bytes;
            if src_end > buffer.len() || dst_end > snap.data.len() {
                break;
            }
            snap.data[snap.len..dst_end].copy_from_slice(&buffer[off..src_end]);
            snap.len = dst_end;
        }
        self.cursor_saved = if snap.len > 0 { Some(snap) } else { None };
    }

    fn restore_cursor_area(&mut self) {
        if let Some(save) = self.cursor_saved.take() {
            let bpp = self.info.bytes_per_pixel;
            let stride = self.info.stride;
            let max_y = self.info.height;
            let mut src_off = 0;
            let copy_w = save.w.min(self.info.width.saturating_sub(save.x));
            if copy_w == 0 || save.h == 0 {
                return;
            }
            {
                let buffer = self.base_buffer_mut();
                for row in 0..save.h {
                    if src_off >= save.len {
                        break;
                    }
                    let py = save.y + row;
                    if py >= max_y {
                        break;
                    }
                    let row_bytes = copy_w * bpp;
                    let dst_off = (py * stride + save.x) * bpp;
                    let dst_end = dst_off + row_bytes;
                    let src_end = src_off + row_bytes;
                    if dst_end > buffer.len() || src_end > save.len {
                        break;
                    }
                    buffer[dst_off..dst_end].copy_from_slice(&save.data[src_off..src_end]);
                    src_off = src_end;
                }
            }
            self.mark_dirty(save.x, save.y, copy_w, save.h);
        }
    }

    fn draw_cursor(&mut self) {
        if !self.cursor_visible {
            return;
        }
        if let Some((px, py, w, h)) = self.cursor_rect() {
            let color = Self::apply_intensity(self.cursor_color, self.bg, self.cursor_intensity);
            self.save_cursor_area(px, py, w, h);
            if self.cursor_saved.is_none() {
                return;
            }
            self.mark_dirty(px, py, w, h);
            self.fill_rect_raw(px, py, w, h, color);
        }
    }

    fn erase_cursor(&mut self) {
        self.restore_cursor_area();
    }

    pub fn cput_char(&mut self, c: char, fg: u32, bg: u32) {
        let old_fg = self.fg;
        let old_bg = self.bg;
        self.fg = fg;
        self.bg = bg;
        self.put_char(c);
        self.fg = old_fg;
        self.bg = old_bg;
    }

    pub fn cwrite(&mut self, s: &str, fg: u32, bg: u32) {
        let old_fg = self.fg;
        let old_bg = self.bg;
        self.fg = fg;
        self.bg = bg;
        for c in s.chars() {
            self.put_char(c);
        }
        self.fg = old_fg;
        self.bg = old_bg;
    }

    pub fn cwrite_line(&mut self, s: &str, fg: u32, bg: u32) {
        let old_fg = self.fg;
        let old_bg = self.bg;
        self.fg = fg;
        self.bg = bg;
        for c in s.chars() {
            self.put_char(c);
        }
        self.fg = old_fg;
        self.bg = old_bg;
        self.put_char('\n');
    }

    pub fn cursor_position(&self) -> (usize, usize) {
        (self.cursor_x, self.cursor_y)
    }

    pub fn move_cursor_to(&mut self, x: usize, y: usize) {
        self.erase_cursor();
        let max_x = self.width.saturating_sub(1);
        let max_y = self.text_area_height().saturating_sub(1);
        self.cursor_x = x.min(max_x);
        self.cursor_y = y.min(max_y);
        self.cursor_visible = true;
        self.cursor_intensity = 255;
        self.draw_cursor();
        self.present();
    }

    pub fn render_line_at(
        &mut self,
        origin_x: usize,
        origin_y: usize,
        content: &str,
        prev_render_len: usize,
        cursor_offset: usize,
        selection: Option<(usize, usize)>,
        suggestion: Option<&str>,
    ) -> usize {
        self.erase_cursor();
        let max_x = self.width;
        let max_y = self.text_area_height();
        if max_x == 0 || max_y == 0 {
            return 0;
        }
        let origin_x = origin_x.min(max_x - 1);
        let start_y = origin_y.min(max_y - 1);
        let mut x = origin_x;
        let mut y = start_y;
        let mut drawn = 0;
        
        for (i, ch) in content.chars().enumerate() {
            if x >= max_x {
                x = 0;
                y += 1;
            }
            if y >= max_y {
                break;
            }
            
            let is_selected = if let Some((start, end)) = selection {
                i >= start && i < end
            } else {
                false
            };

            if is_selected {
                let old_bg = self.bg;
                self.bg = self.fg;
                self.draw_glyph(x, y, ch, old_bg);
                self.bg = old_bg;
            } else {
                self.draw_glyph(x, y, ch, self.fg);
            }

            x += 1;
            drawn += 1;
        }

        if let Some(sugg) = suggestion {
            let ghost_color = Self::apply_intensity(self.fg, self.bg, 100);
            for ch in sugg.chars() {
                if x >= max_x {
                    x = 0;
                    y += 1;
                }
                if y >= max_y {
                    break;
                }
                self.draw_glyph(x, y, ch, ghost_color);
                x += 1;
                drawn += 1;
            }
        }

        let trailing = prev_render_len.saturating_sub(drawn);
        for _ in 0..trailing {
            if x >= max_x {
                x = 0;
                y += 1;
            }
            if y >= max_y {
                break;
            }
            self.draw_glyph(x, y, ' ', self.bg);
            x += 1;
        }
        
        
        let total_offset = origin_x.saturating_add(cursor_offset);
        let cursor_wrap_x = total_offset % max_x;
        let cursor_wrap_y = start_y + (total_offset / max_x);
        
        self.cursor_x = cursor_wrap_x;
        self.cursor_y = cursor_wrap_y.min(max_y - 1);
        
        self.cursor_visible = true;
        self.cursor_intensity = 255;
        self.draw_cursor();
        self.present();
        drawn
    }

    pub fn size(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    pub fn reserve_hud_rows(&mut self, rows: usize) {
        let rows = rows.min(self.height);
        self.reserved_hud_rows = rows;
        self.refresh_hud_layer();
    }

    fn text_area_height(&self) -> usize {
        self.height.saturating_sub(self.reserved_hud_rows)
    }

    fn layer_ref(&self, id: LayerId) -> Option<&Layer> {
        self.layers.get(id.idx()).and_then(|l| l.as_ref())
    }

    fn layer_mut(&mut self, id: LayerId) -> Option<&mut Layer> {
        self.layers.get_mut(id.idx()).and_then(|l| l.as_mut())
    }

    fn create_layer_with_blend(
        &mut self,
        width: usize,
        height: usize,
        x: usize,
        y: usize,
        z: i16,
        alpha: u8,
        blend: BlendMode,
    ) -> Option<LayerId> {
        if width == 0 || height == 0 {
            return None;
        }
        let bpp = self.info.bytes_per_pixel;
        let stride = width;
        let size = stride.checked_mul(height)?.checked_mul(bpp)?;
        let buffer = LayerBuffer::new_heap(size)?;
        let slot = self.layers.iter().position(|l| l.is_none())?;
        self.layers[slot] = Some(Layer {
            buffer,
            width,
            height,
            stride,
            x,
            y,
            alpha,
            visible: true,
            z,
            blend,
            dirty: None,
        });
        Some(LayerId(slot as u8))
    }

    pub fn create_layer(
        &mut self,
        width: usize,
        height: usize,
        x: usize,
        y: usize,
        z: i16,
        alpha: u8,
    ) -> Option<LayerId> {
        let blend = if alpha >= 255 {
            BlendMode::Opaque
        } else {
            BlendMode::Alpha
        };
        self.create_layer_with_blend(width, height, x, y, z, alpha, blend)
    }

    fn create_layer_in_app_heap(
        &mut self,
        width: usize,
        height: usize,
        x: usize,
        y: usize,
        z: i16,
        alpha: u8,
        app_id: memory::AppId,
    ) -> Option<LayerId> {
        if width == 0 || height == 0 {
            return None;
        }
        let bpp = self.info.bytes_per_pixel;
        let stride = width;
        let size = stride.checked_mul(height)?.checked_mul(bpp)?;
        let align = core::mem::align_of::<u64>();
        let buffer = unsafe { LayerBuffer::new_app(app_id, size, align)? };
        let blend = if alpha >= 255 { BlendMode::Opaque } else { BlendMode::Alpha };
        let slot = self.layers.iter().position(|l| l.is_none())?;
        self.layers[slot] = Some(Layer {
            buffer,
            width,
            height,
            stride,
            x,
            y,
            alpha,
            visible: true,
            z,
            blend,
            dirty: None,
        });
        Some(LayerId(slot as u8))
    }

    pub fn destroy_layer(&mut self, id: LayerId) {
        if let Some(layer) = self.layer_ref(id) {
            if layer.visible {
                self.mark_dirty(layer.x, layer.y, layer.width, layer.height);
            }
        }
        if self.hud_layer == Some(id) {
            self.hud_layer = None;
        }
        if self.image_layer == Some(id) {
            self.image_layer = None;
        }
        if let Some(slot) = self.layers.get_mut(id.idx()) {
            *slot = None;
        }
    }

    pub fn layer_set_visible(&mut self, id: LayerId, visible: bool) {
        let mut dirty = None;
        if let Some(layer) = self.layer_mut(id) {
            if layer.visible != visible {
                layer.visible = visible;
                if visible {
                    layer.dirty = None;
                }
                dirty = Some((layer.x, layer.y, layer.width, layer.height));
            }
        }
        if let Some((x, y, w, h)) = dirty {
            self.mark_dirty(x, y, w, h);
        }
    }

    pub fn layer_set_alpha(&mut self, id: LayerId, alpha: u8) {
        let mut dirty = None;
        if let Some(layer) = self.layer_mut(id) {
            if layer.alpha != alpha {
                layer.alpha = alpha;
                layer.blend = if alpha >= 255 { BlendMode::Opaque } else { BlendMode::Alpha };
                if layer.visible {
                    dirty = Some((layer.x, layer.y, layer.width, layer.height));
                }
            }
        }
        if let Some((x, y, w, h)) = dirty {
            self.mark_dirty(x, y, w, h);
        }
    }

    pub fn layer_set_pos(&mut self, id: LayerId, x: usize, y: usize) {
        let mut old = None;
        let mut new = None;
        if let Some(layer) = self.layer_mut(id) {
            if layer.x != x || layer.y != y {
                old = Some((layer.x, layer.y, layer.width, layer.height));
                layer.x = x;
                layer.y = y;
                new = Some((x, y, layer.width, layer.height));
            }
        }
        if let Some((x0, y0, w, h)) = old {
            self.mark_dirty(x0, y0, w, h);
        }
        if let Some((x1, y1, w, h)) = new {
            self.mark_dirty(x1, y1, w, h);
        }
    }

    pub fn layer_set_z(&mut self, id: LayerId, z: i16) {
        let mut dirty = None;
        if let Some(layer) = self.layer_mut(id) {
            if layer.z != z {
                layer.z = z;
                if layer.visible {
                    dirty = Some((layer.x, layer.y, layer.width, layer.height));
                }
            }
        }
        if let Some((x, y, w, h)) = dirty {
            self.mark_dirty(x, y, w, h);
        }
    }

    pub fn layer_clear(&mut self, id: LayerId, color: u32) {
        let pixel_format = self.info.pixel_format;
        let bpp = self.info.bytes_per_pixel;
        if let Some(layer) = self.layer_mut(id) {
            let width = layer.width;
            let height = layer.height;
            let stride = layer.stride;
            {
                let buf = layer.buffer.as_mut_slice();
                fill_rect_into_raw(
                    buf,
                    stride,
                    height,
                    0,
                    0,
                    width,
                    height,
                    color,
                    pixel_format,
                    bpp,
                );
            }
            let x1 = layer.width;
            let y1 = layer.height;
            merge_dirty_rect(&mut layer.dirty, 0, 0, x1, y1);
        }
    }

    pub fn layer_fill_rect(&mut self, id: LayerId, x: usize, y: usize, w: usize, h: usize, color: u32) {
        let pixel_format = self.info.pixel_format;
        let bpp = self.info.bytes_per_pixel;
        if let Some(layer) = self.layer_mut(id) {
            if w == 0 || h == 0 {
                return;
            }
            let x0 = x.min(layer.width);
            let y0 = y.min(layer.height);
            let x1 = x.saturating_add(w).min(layer.width);
            let y1 = y.saturating_add(h).min(layer.height);
            if x1 > x0 && y1 > y0 {
                let w_px = x1 - x0;
                let h_px = y1 - y0;
                let stride = layer.stride;
                let height = layer.height;
                {
                    let buf = layer.buffer.as_mut_slice();
                    fill_rect_into_raw(
                        buf,
                        stride,
                        height,
                        x0,
                        y0,
                        w_px,
                        h_px,
                        color,
                        pixel_format,
                        bpp,
                    );
                }
                let x1 = x0 + w_px;
                let y1 = y0 + h_px;
                merge_dirty_rect(&mut layer.dirty, x0, y0, x1, y1);
            }
        }
    }

    pub fn layer_draw_text_at_char(&mut self, id: LayerId, x: usize, y: usize, s: &str, fg: u32, bg: u32) {
        let font = self.font();
        let scale = self.scale;
        let pixel_format = self.info.pixel_format;
        let bpp = self.info.bytes_per_pixel;
        let char_w = font.width * scale;
        let char_h = font.height * scale;
        if char_w == 0 || char_h == 0 {
            return;
        }
        if let Some(layer) = self.layer_mut(id) {
            let max_cols = layer.width / char_w;
            let max_rows = layer.height / char_h;
            if max_cols == 0 || max_rows == 0 {
                return;
            }
            let stride = layer.stride;
            let height = layer.height;
            let mut cx = x;
            let mut cy = y;
            let mut min_x = usize::MAX;
            let mut min_y = usize::MAX;
            let mut max_x = 0usize;
            let mut max_y = 0usize;
            let mut any = false;
            {
                let buf = layer.buffer.as_mut_slice();
                for ch in s.chars() {
                    if cx >= max_cols {
                        cx = 0;
                        cy += 1;
                    }
                    if cy >= max_rows {
                        break;
                    }
                    draw_glyph_into_raw(
                        buf,
                        stride,
                        height,
                        font,
                        scale,
                        cx,
                        cy,
                        ch,
                        fg,
                        bg,
                        pixel_format,
                        bpp,
                    );
                    if !any {
                        min_x = cx;
                        min_y = cy;
                        max_x = cx;
                        max_y = cy;
                        any = true;
                    } else {
                        min_x = min_x.min(cx);
                        min_y = min_y.min(cy);
                        max_x = max_x.max(cx);
                        max_y = max_y.max(cy);
                    }
                    cx += 1;
                }
            }
            if any {
                let x_px = min_x.saturating_mul(char_w);
                let y_px = min_y.saturating_mul(char_h);
                let w_px = (max_x.saturating_sub(min_x).saturating_add(1)).saturating_mul(char_w);
                let h_px = (max_y.saturating_sub(min_y).saturating_add(1)).saturating_mul(char_h);
                let x1 = (x_px + w_px).min(layer.width);
                let y1 = (y_px + h_px).min(layer.height);
                merge_dirty_rect(&mut layer.dirty, x_px, y_px, x1, y1);
            }
        }
    }

    pub fn layer_scroll_rect(
        &mut self,
        id: LayerId,
        x: usize,
        y: usize,
        w: usize,
        h: usize,
        dy: i32,
        fill: u32,
    ) {
        if dy == 0 || w == 0 || h == 0 {
            return;
        }
        let bpp = self.info.bytes_per_pixel;
        let pixel_format = self.info.pixel_format;
        let Some(layer) = self.layer_mut(id) else { return; };
        if x >= layer.width || y >= layer.height {
            return;
        }
        let w = w.min(layer.width.saturating_sub(x));
        let h = h.min(layer.height.saturating_sub(y));
        if w == 0 || h == 0 {
            return;
        }
        let stride = layer.stride;
        let height = layer.height;
        let row_bytes = w.saturating_mul(bpp);
        let buf = layer.buffer.as_mut_slice();

        if dy < 0 {
            let shift = (-dy) as usize;
            if shift >= h {
                fill_rect_into_raw(buf, stride, height, x, y, w, h, fill, pixel_format, bpp);
            } else {
                for row in 0..(h - shift) {
                    let src_y = y + row + shift;
                    let dst_y = y + row;
                    let src = (src_y.saturating_mul(stride).saturating_add(x)).saturating_mul(bpp);
                    let dst = (dst_y.saturating_mul(stride).saturating_add(x)).saturating_mul(bpp);
                    buf.copy_within(src..src.saturating_add(row_bytes), dst);
                }
                fill_rect_into_raw(
                    buf,
                    stride,
                    height,
                    x,
                    y + h - shift,
                    w,
                    shift,
                    fill,
                    pixel_format,
                    bpp,
                );
            }
        } else {
            let shift = dy as usize;
            if shift >= h {
                fill_rect_into_raw(buf, stride, height, x, y, w, h, fill, pixel_format, bpp);
            } else {
                for row in (0..(h - shift)).rev() {
                    let src_y = y + row;
                    let dst_y = y + row + shift;
                    let src = (src_y.saturating_mul(stride).saturating_add(x)).saturating_mul(bpp);
                    let dst = (dst_y.saturating_mul(stride).saturating_add(x)).saturating_mul(bpp);
                    buf.copy_within(src..src.saturating_add(row_bytes), dst);
                }
                fill_rect_into_raw(buf, stride, height, x, y, w, shift, fill, pixel_format, bpp);
            }
        }
        let x1 = (x + w).min(layer.width);
        let y1 = (y + h).min(layer.height);
        merge_dirty_rect(&mut layer.dirty, x, y, x1, y1);
    }

    fn refresh_hud_layer(&mut self) {
        if self.compositor_mode != CompositorMode::Layered {
            if let Some(id) = self.hud_layer.take() {
                self.destroy_layer(id);
            }
            return;
        }
        if self.reserved_hud_rows == 0 {
            if let Some(id) = self.hud_layer.take() {
                self.destroy_layer(id);
            }
            return;
        }
        let hud_h_px = self.reserved_hud_rows * self.char_h();
        if hud_h_px == 0 {
            return;
        }
        let hud_y = self.info.height.saturating_sub(hud_h_px);
        let needs_new = match self.hud_layer.and_then(|id| self.layer_ref(id)) {
            Some(layer) => {
                layer.width != self.info.width || layer.height != hud_h_px || layer.x != 0 || layer.y != hud_y
            }
            None => true,
        };
        if needs_new {
            if let Some(id) = self.hud_layer.take() {
                self.destroy_layer(id);
            }
            let id = self.create_layer_with_blend(
                self.info.width,
                hud_h_px,
                0,
                hud_y,
                HUD_LAYER_Z,
                255,
                BlendMode::Opaque,
            );
            self.hud_layer = id;
            if let Some(id) = self.hud_layer {
                self.layer_clear(id, self.bg);
            }
        }
    }

    fn ensure_image_layer(&mut self) -> Option<LayerId> {
        if self.compositor_mode != CompositorMode::Layered {
            return None;
        }
        let needs_new = match self.image_layer.and_then(|id| self.layer_ref(id)) {
            Some(layer) => layer.width != self.info.width || layer.height != self.info.height || layer.x != 0 || layer.y != 0,
            None => true,
        };
        if needs_new {
            if let Some(id) = self.image_layer.take() {
                self.destroy_layer(id);
            }
            let id = self.create_layer_with_blend(
                self.info.width,
                self.info.height,
                0,
                0,
                IMAGE_LAYER_Z,
                255,
                BlendMode::Opaque,
            );
            self.image_layer = id;
            if let Some(id) = self.image_layer {
                if let Some(layer) = self.layer_mut(id) {
                    layer.visible = false;
                }
            }
        }
        self.image_layer
    }

    pub fn draw_text_at_char(&mut self, pos: DrawPos, s: &str) {
        match pos {
            DrawPos::Char(x, y) => {
                let old_x = self.cursor_x;
                let old_y = self.cursor_y;
                let old_visible = self.cursor_visible;
                self.erase_cursor();
                let mut cx = x;
                for ch in s.chars() {
                    self.draw_glyph(cx, y, ch, self.fg);
                    cx += 1;
                }
                self.cursor_x = old_x;
                self.cursor_y = old_y;
                self.cursor_visible = old_visible;
                self.draw_cursor();
            }
        }
        self.present();
    }

    pub fn hud_begin(&mut self) {
        if self.reserved_hud_rows == 0 {
            return;
        }
        if self.compositor_mode == CompositorMode::Layered {
            self.refresh_hud_layer();
            if let Some(id) = self.hud_layer {
                self.layer_clear(id, self.bg);
            }
            return;
        }
        let hud_h_px = self.reserved_hud_rows * self.char_h();
        let start_y = self.info.height.saturating_sub(hud_h_px);
        self.fill_rect(0, start_y, self.info.width, hud_h_px, self.bg);
    }

    pub fn hud_draw_text(&mut self, s: &str, fg: u32, align: HudAlign) {
        if self.reserved_hud_rows == 0 { return; }
        if self.compositor_mode == CompositorMode::Layered {
            self.refresh_hud_layer();
            let Some(id) = self.hud_layer else { return; };
            let font = self.font();
            let scale = self.scale;
            let char_w = font.width * scale;
            let char_h = font.height * scale;
            let bg = self.bg;
            let pixel_format = self.info.pixel_format;
            let bpp = self.info.bytes_per_pixel;
            if char_w == 0 || char_h == 0 {
                return;
            }
            let text_chars = s.chars().count();
            if let Some(layer) = self.layer_mut(id) {
                let max_chars = layer.width / char_w;
                let stride = layer.stride;
                let height = layer.height;
                let x_char = match align {
                    HudAlign::Left => 0,
                    HudAlign::Center => (max_chars / 2).saturating_sub(text_chars / 2),
                    HudAlign::Right => max_chars.saturating_sub(text_chars),
                };
                let y_char = 0;
                let mut cx = x_char;
                let mut drawn = 0usize;
                {
                    let buf = layer.buffer.as_mut_slice();
                    for ch in s.chars() {
                        if cx >= max_chars {
                            break;
                        }
                        draw_glyph_into_raw(
                            buf,
                            stride,
                            height,
                            font,
                            scale,
                            cx,
                            y_char,
                            ch,
                            fg,
                            bg,
                            pixel_format,
                            bpp,
                        );
                        cx += 1;
                        drawn += 1;
                    }
                }
                if drawn > 0 {
                    let x_px = x_char.saturating_mul(char_w);
                    let w_px = drawn.saturating_mul(char_w);
                    let h_px = char_h;
                    let x1 = (x_px + w_px).min(layer.width);
                    let y1 = h_px.min(layer.height);
                    merge_dirty_rect(&mut layer.dirty, x_px, 0, x1, y1);
                }
            }
            return;
        }
        let scale = self.scale;
        let font = self.font();
        let char_w = font.width * scale;
        if char_w == 0 {
            return;
        }
        let text_chars = s.chars().count();
        let y_char = self.height.saturating_sub(self.reserved_hud_rows);
        let x_char = match align {
            HudAlign::Left => 0,
            HudAlign::Center => ((self.info.width / char_w) / 2).saturating_sub(text_chars / 2),
            HudAlign::Right => (self.info.width / char_w).saturating_sub(text_chars),
        };
        let mut cx = x_char;
        for ch in s.chars() {
            self.draw_glyph(cx, y_char, ch, fg);
            cx += 1;
        }
    }

    pub fn hud_present(&mut self) {
        if self.reserved_hud_rows == 0 { return; }
        self.present();
    }

    pub fn clear_hud_row(&mut self) {
        self.hud_begin();
        self.hud_present();
    }

    pub fn erase_hud_box_for_len(&mut self, len: usize) {
        self.hud_begin();
        self.hud_present();
    }

    pub fn set_cursor_style(&mut self, style: CursorStyle) {
        self.erase_cursor();
        self.cursor_style = style;
        self.cursor_visible = !matches!(style, CursorStyle::Hidden);
        self.cursor_intensity = 255;
        self.draw_cursor();
        self.present();
    }

    pub fn set_cursor_blink(&mut self, blink: CursorBlink) {
        self.erase_cursor();
        self.cursor_blink = blink;
        self.cursor_intensity = 255;
        self.cursor_visible = true;
        self.blink_timer = 0;
        self.draw_cursor();
        self.present();
    }

    pub fn set_cursor_color(&mut self, color: u32) {
        self.cursor_color = color;
    }

    pub fn set_font(&mut self, kind: FontKind) {
        if self.font_kind == kind {
            return;
        }
        self.font_kind = kind;
        self.scale = kind.default_scale();
        self.recompute_dimensions();
        self.clear();
    }

    pub fn current_font(&self) -> FontKind {
        self.font_kind
    }

    pub fn cursor_style(&self) -> CursorStyle {
        self.cursor_style
    }

    pub fn cursor_blink(&self) -> CursorBlink {
        self.cursor_blink
    }

    pub fn cursor_color(&self) -> u32 {
        self.cursor_color
    }

    pub fn reserved_hud_rows(&self) -> usize {
        self.reserved_hud_rows
    }

    pub fn compositor_mode(&self) -> CompositorMode {
        self.compositor_mode
    }

    pub fn is_double_buffered(&self) -> bool {
        self.double_buffered
    }

    pub fn is_classic_mode(&self) -> bool {
        self.classic_mode
    }

    pub fn set_classic_mode(&mut self, enabled: bool) {
        if self.classic_mode == enabled {
            return;
        }
        self.classic_mode = enabled;
        if enabled {
            self.clear_row_cache = None;
        }
    }

    pub fn has_scene_buffer(&self) -> bool {
        self.scene_buffer.is_some()
    }

    pub fn toggle_double_buffering(&mut self) -> Result<bool, &'static str> {
        let enable = !self.double_buffered;
        self.set_double_buffering(enable)
    }

    pub fn set_double_buffering(&mut self, enabled: bool) -> Result<bool, &'static str> {
        if enabled == self.double_buffered {
            return Ok(self.double_buffered);
        }

        if enabled {
            let len = self.fb.len();
            let mut back_buffer = alloc_back_buffer(len)
                .ok_or("fbffer: back buffer unavailable.")?;
            let dst = back_buffer.as_mut_slice();
            let copy_len = core::cmp::min(dst.len(), len);
            dst[..copy_len].copy_from_slice(&self.fb[..copy_len]);
            self.back_buffer = Some(back_buffer);
            self.double_buffered = true;
            self.mark_dirty(0, 0, self.info.width, self.info.height);
            self.present_full();
            Ok(true)
        } else {
            if self.double_buffered {
                self.present_full();
            }
            self.double_buffered = false;
            self.back_buffer = None;
            Ok(false)
        }
    }

    pub fn set_compositor_mode(&mut self, mode: CompositorMode) {
        if self.compositor_mode == mode {
            return;
        }
        if mode == CompositorMode::Layered && self.scene_buffer.is_none() {
            return;
        }
        self.erase_cursor();
        match mode {
            CompositorMode::Legacy => {
                if let Some(scene) = self.scene_buffer.as_deref() {
                    let target = if self.double_buffered {
                        match self.back_buffer.as_mut() {
                            Some(buffer) => buffer.as_mut_slice(),
                            None => &mut *self.fb,
                        }
                    } else {
                        &mut *self.fb
                    };
                    let len = core::cmp::min(scene.len(), target.len());
                    target[..len].copy_from_slice(&scene[..len]);
                }
            }
            CompositorMode::Layered => {
                if let Some(scene) = self.scene_buffer.as_deref_mut() {
                    let source = if self.double_buffered {
                        self.back_buffer
                            .as_ref()
                            .map(|buffer| buffer.as_slice())
                            .unwrap_or(&*self.fb)
                    } else {
                        &*self.fb
                    };
                    let len = core::cmp::min(scene.len(), source.len());
                    scene[..len].copy_from_slice(&source[..len]);
                }
            }
        }
        self.compositor_mode = mode;
        self.refresh_hud_layer();
        self.mark_dirty(0, 0, self.info.width, self.info.height);
        self.draw_cursor();
        self.present_full();
    }

    pub fn set_default_fg(&mut self, fg: u32) {
        self.fg = fg;
        self.cursor_color = fg;
    }

    pub fn set_default_bg(&mut self, bg: u32) {
        if self.bg == bg {
            return;
        }
        self.bg = bg;
        self.clear();
    }

    pub fn set_default_colors(&mut self, fg: u32, bg: u32) {
        let bg_changed = self.bg != bg;
        self.fg = fg;
        self.bg = bg;
        self.cursor_color = fg;
        if bg_changed {
            self.clear();
        }
    }

    pub fn default_colors(&self) -> (u32, u32) {
        (self.fg, self.bg)
    }

    pub fn tick(&mut self) {
        if matches!(self.cursor_style, CursorStyle::Hidden) && matches!(self.cursor_blink, CursorBlink::None) {
            return;
        }
        match self.cursor_blink {
            CursorBlink::None => {
                self.cursor_visible = true;
                self.cursor_intensity = 255;
            }
            CursorBlink::Pulse => {
                self.blink_timer = self.blink_timer.wrapping_add(1);
                if self.blink_timer % 60 == 0 {
                    self.cursor_visible = !self.cursor_visible;
                }
            }
            CursorBlink::Fade => {
                self.blink_timer = self.blink_timer.wrapping_add(1);
                let t = self.blink_timer as f32;
                let x = (t / 240.0) * core::f32::consts::PI * 2.0;
                let v = ((1.0 - libm::cosf(x)) * 0.5) * 255.0;
                self.cursor_intensity = v as u8;
                self.cursor_visible = self.cursor_intensity > 4;
            }
        }
        self.erase_cursor();
        self.draw_cursor();
        self.present();
    }

    fn blit_image_scaled_into(
        &mut self,
        dst: &mut [u8],
        dst_w: usize,
        dst_h: usize,
        dst_stride: usize,
        image: &[u8],
        img_w: usize,
        img_h: usize,
        channels: usize,
    ) {
        blit_image_scaled_into_raw(
            dst,
            dst_w,
            dst_h,
            dst_stride,
            image,
            img_w,
            img_h,
            channels,
            self.info.pixel_format,
            self.info.bytes_per_pixel,
        );
    }
}

pub static CONSOLE: Mutex<Option<Console>> = Mutex::new(None);
type OutputHook = fn(&str, bool);
static OUTPUT_HOOK: Mutex<Option<OutputHook>> = Mutex::new(None);

pub fn init_console(boot: &'static mut BootInfo) {
    if let Some(console) = Console::from_boot_info(boot) {
        *CONSOLE.lock() = Some(console);
    }
}

pub fn with_console<F, R>(f: F) -> R
where
    F: FnOnce(&mut Console) -> R,
{
    interrupts::without_interrupts(|| {
        let mut lock = CONSOLE.lock();
        let con = lock.as_mut().expect("Console not init");
        f(con)
    })
}

fn output_hook() -> Option<OutputHook> {
    *OUTPUT_HOOK.lock()
}

pub fn set_output_hook(hook: Option<OutputHook>) {
    *OUTPUT_HOOK.lock() = hook;
}

pub fn write_line(s: &str) {
    if let Some(hook) = output_hook() {
        hook(s, true);
        return;
    }
    with_console(|c| c.write_line(s));
}

pub fn write_inline(s: &str) {
    if let Some(hook) = output_hook() {
        hook(s, true);
        return;
    }
    with_console(|c| c.write_inline(s));
}

pub fn clear_screen() {
    with_console(|c| c.clear());
}

pub fn clear_text_area() {
    with_console(|c| c.clear_text_area());
}

pub fn present() {
    with_console(|c| c.present());
}

pub fn cwrite_line(s: &str, fg: u32, bg: u32) {
    if let Some(hook) = output_hook() {
        hook(s, true);
        return;
    }
    with_console(|c| c.cwrite_line(s, fg, bg));
}

pub fn cput_char(c: char, fg: u32, bg: u32) {
    with_console(|con| con.cput_char(c, fg, bg));
}

pub fn cwrite(s: &str, fg: u32, bg: u32) {
    if let Some(hook) = output_hook() {
        hook(s, false);
        return;
    }
    with_console(|c| c.cwrite(s, fg, bg));
}

pub fn write(s: &str) {
    if let Some(hook) = output_hook() {
        hook(s, false);
        return;
    }
    with_console(|c| c.write(s));
}

pub fn set_cursor_style(style: CursorStyle) {
    with_console(|c| c.set_cursor_style(style));
}

pub fn set_cursor_blink(blink: CursorBlink) {
    with_console(|c| c.set_cursor_blink(blink));
}

pub fn set_cursor_color(color: u32) {
    with_console(|c| c.set_cursor_color(color));
}

pub fn cursor_style() -> CursorStyle {
    with_console(|c| c.cursor_style())
}

pub fn cursor_blink() -> CursorBlink {
    with_console(|c| c.cursor_blink())
}

pub fn cursor_color() -> u32 {
    with_console(|c| c.cursor_color())
}

pub fn set_font(kind: FontKind) {
    with_console(|c| c.set_font(kind));
}

pub fn current_font() -> FontKind {
    with_console(|c| c.current_font())
}

pub fn compositor_mode() -> CompositorMode {
    with_console(|c| c.compositor_mode())
}

pub fn is_double_buffered() -> bool {
    with_console(|c| c.is_double_buffered())
}

pub fn is_classic_mode() -> bool {
    with_console(|c| c.is_classic_mode())
}

pub fn set_classic_mode(enabled: bool) {
    with_console(|c| c.set_classic_mode(enabled));
}

pub fn reserve_hud_rows(rows: usize) {
    with_console(|c| c.reserve_hud_rows(rows));
}

pub fn reserved_hud_rows() -> usize {
    with_console(|c| c.reserved_hud_rows())
}

pub fn has_scene_buffer() -> bool {
    with_console(|c| c.has_scene_buffer())
}

pub fn set_compositor_mode(mode: CompositorMode) {
    with_console(|c| c.set_compositor_mode(mode));
}

pub fn toggle_double_buffering() -> Result<bool, &'static str> {
    with_console(|c| c.toggle_double_buffering())
}

pub fn set_double_buffering(enabled: bool) -> Result<bool, &'static str> {
    with_console(|c| c.set_double_buffering(enabled))
}

pub fn create_layer(
    width: usize,
    height: usize,
    x: usize,
    y: usize,
    z: i16,
    alpha: u8,
) -> Option<LayerId> {
    with_console(|c| c.create_layer(width, height, x, y, z, alpha))
}

pub fn create_layer_in_app_heap(
    width: usize,
    height: usize,
    x: usize,
    y: usize,
    z: i16,
    alpha: u8,
    app_id: memory::AppId,
) -> Option<LayerId> {
    with_console(|c| c.create_layer_in_app_heap(width, height, x, y, z, alpha, app_id))
}

pub fn destroy_layer(id: LayerId) {
    with_console(|c| c.destroy_layer(id));
}

pub fn layer_set_visible(id: LayerId, visible: bool) {
    with_console(|c| c.layer_set_visible(id, visible));
}

pub fn layer_set_alpha(id: LayerId, alpha: u8) {
    with_console(|c| c.layer_set_alpha(id, alpha));
}

pub fn layer_set_pos(id: LayerId, x: usize, y: usize) {
    with_console(|c| c.layer_set_pos(id, x, y));
}

pub fn layer_set_z(id: LayerId, z: i16) {
    with_console(|c| c.layer_set_z(id, z));
}

pub fn layer_clear(id: LayerId, color: u32) {
    with_console(|c| c.layer_clear(id, color));
}

pub fn layer_fill_rect(id: LayerId, x: usize, y: usize, w: usize, h: usize, color: u32) {
    with_console(|c| c.layer_fill_rect(id, x, y, w, h, color));
}

pub fn set_resize_outline(x: usize, y: usize, w: usize, h: usize, thickness: usize, color: u32) {
    with_console(|c| {
        c.set_outline_overlay(Some(OutlineOverlay {
            x,
            y,
            w,
            h,
            color,
            thickness,
        }));
    });
}

pub fn clear_resize_outline() {
    with_console(|c| c.set_outline_overlay(None));
}

pub fn layer_draw_text_at_char(id: LayerId, x: usize, y: usize, s: &str, fg: u32, bg: u32) {
    with_console(|c| c.layer_draw_text_at_char(id, x, y, s, fg, bg));
}

pub fn layer_scroll_rect(id: LayerId, x: usize, y: usize, w: usize, h: usize, dy: i32, fill: u32) {
    with_console(|c| c.layer_scroll_rect(id, x, y, w, h, dy, fill));
}

pub fn set_default_fg(color: u32) {
    with_console(|c| c.set_default_fg(color));
}

pub fn set_default_bg(color: u32) {
    with_console(|c| c.set_default_bg(color));
}

pub fn set_default_colors(fg: u32, bg: u32) {
    with_console(|c| c.set_default_colors(fg, bg));
}

pub fn default_colors() -> (u32, u32) {
    with_console(|c| c.default_colors())
}

pub fn default_fg() -> u32 {
    default_colors().0
}

pub fn default_bg() -> u32 {
    default_colors().1
}

pub fn size_chars() -> (usize, usize) {
    with_console(|c| (c.width, c.text_area_height()))
}

pub fn render_line_at(
    origin_x: usize,
    origin_y: usize,
    content: &str,
    prev_render_len: usize,
    cursor_offset: usize,
    selection: Option<(usize, usize)>,
    suggestion: Option<&str>,
) -> usize {
    with_console(|c| c.render_line_at(origin_x, origin_y, content, prev_render_len, cursor_offset, selection, suggestion))
}

pub fn tick() {
    with_console(|c| c.tick());
}

pub fn display_buffer_stats() -> Option<DisplayBufferStats> {
    interrupts::without_interrupts(|| {
        let lock = CONSOLE.lock();
        lock.as_ref().map(|c| c.buffer_stats())
    })
}

fn infer_image_dims(image: &[u8], fb_w: usize, fb_h: usize) -> Option<(usize, usize, usize)> {
    let channels = if image.len() % 4 == 0 { 4 } else if image.len() % 3 == 0 { 3 } else { return None };
    let total_px = image.len() / channels;
    let target_aspect = fb_w as f32 / fb_h as f32;
    let mut best: Option<(usize, usize, f32)> = None;
    let limit = libm::sqrtf(total_px as f32) as usize + 1;
    for w in 1..=limit {
        if total_px % w != 0 {
            continue;
        }
        let h = total_px / w;
        let aspect = w as f32 / h as f32;
        let diff = libm::fabsf(aspect - target_aspect);
        match best {
            None => best = Some((w, h, diff)),
            Some((_, _, best_diff)) if diff < best_diff => best = Some((w, h, diff)),
            _ => {}
        }
    }
    best.map(|(w, h, _)| (w, h, channels))
}

pub fn showimage(image: &[u8], width: usize, height: usize, seconds: u64) {
    let state: Option<(
        CompositorMode,
        Option<LayerId>,
        usize,
        CursorStyle,
        bool,
        CursorBlink,
        bool,
    )> =
        interrupts::without_interrupts(|| {
            let mut lock = CONSOLE.lock();
            let con = lock.as_mut().expect("Console not init");

            let (w, h, channels) = if width.saturating_mul(height).saturating_mul(4) == image.len() {
                (width, height, 4)
            } else if width.saturating_mul(height).saturating_mul(3) == image.len() {
                (width, height, 3)
            } else {
                infer_image_dims(image, con.info.width, con.info.height).unwrap_or((0, 0, 0))
            };
            if w == 0 || h == 0 || channels == 0 {
                return None;
            }

            let prev_cursor_style = con.cursor_style;
            let prev_cursor_visible = con.cursor_visible;
            let prev_cursor_blink = con.cursor_blink;

            match con.compositor_mode {
                CompositorMode::Layered => {
                    con.erase_cursor();
                    con.cursor_style = CursorStyle::Hidden;
                    con.cursor_visible = false;
                    con.cursor_blink = CursorBlink::None;
                    let prev_present_suspended = con.present_suspended;
                    con.present_suspended = true;
                    let fb_w = con.info.width;
                    let fb_h = con.info.height;
                    let stride = con.info.stride;
                    let pixel_format = con.info.pixel_format;
                    let bpp = con.info.bytes_per_pixel;
                    blit_image_scaled_into_raw(
                        &mut con.fb,
                        fb_w,
                        fb_h,
                        stride,
                        image,
                        w,
                        h,
                        channels,
                        pixel_format,
                        bpp,
                    );
                    Some((
                        CompositorMode::Layered,
                        None,
                        0,
                        prev_cursor_style,
                        prev_cursor_visible,
                        prev_cursor_blink,
                        prev_present_suspended,
                    ))
                }
                CompositorMode::Legacy => {
                    con.erase_cursor();
                    con.cursor_style = CursorStyle::Hidden;
                    con.cursor_visible = false;
                    con.cursor_blink = CursorBlink::None;
                    let len = {
                        let base = if con.double_buffered {
                            con.back_buffer
                                .as_ref()
                                .map(|buffer| buffer.as_slice())
                                .unwrap_or(&*con.fb)
                        } else {
                            &*con.fb
                        };
                        let base_len = base.len();
                        let snapshot = con.scene_buffer.as_deref_mut()?;
                        if snapshot.len() < base_len {
                            return None;
                        }
                        snapshot[..base_len].copy_from_slice(&base[..base_len]);
                        base_len
                    };
                    let fb_w = con.info.width;
                    let fb_h = con.info.height;
                    let stride = con.info.stride;
                    let pixel_format = con.info.pixel_format;
                    let bpp = con.info.bytes_per_pixel;
                    blit_image_scaled_into_raw(
                        con.base_buffer_mut(),
                        fb_w,
                        fb_h,
                        stride,
                        image,
                        w,
                        h,
                        channels,
                        pixel_format,
                        bpp,
                    );
                    con.present_full();
                    Some((
                        CompositorMode::Legacy,
                        None,
                        len,
                        prev_cursor_style,
                        prev_cursor_visible,
                        prev_cursor_blink,
                        con.present_suspended,
                    ))
                }
            }
        });

    let Some((mode, layer_id, snapshot_len, prev_style, prev_visible, prev_blink, prev_present)) = state else {
        return;
    };

    wait::bsec(seconds);

    interrupts::without_interrupts(|| {
        let mut lock = CONSOLE.lock();
        if let Some(con) = lock.as_mut() {
            con.present_suspended = prev_present;
            match mode {
                CompositorMode::Layered => {
                    let _ = layer_id;
                }
                CompositorMode::Legacy => {
                    if snapshot_len > 0 {
                        if let Some(scene) = con.scene_buffer.as_deref() {
                            let target = if con.double_buffered {
                                match con.back_buffer.as_mut() {
                                    Some(buffer) => buffer.as_mut_slice(),
                                    None => &mut *con.fb,
                                }
                            } else {
                                &mut *con.fb
                            };
                            let len = core::cmp::min(snapshot_len, target.len());
                            target[..len].copy_from_slice(&scene[..len]);
                        }
                    }
                }
            }
            con.cursor_style = prev_style;
            con.cursor_visible = prev_visible;
            con.cursor_blink = prev_blink;
            con.cursor_intensity = 255;
            con.draw_cursor();
            con.present_full();
        }
    });
}
