use alloc::boxed::Box;
use alloc::string::String;
use crate::console::{self, LayerId};
use crate::keyboard::KeyEvent;
use heapless::String as HString;

#[derive(Copy, Clone)]
pub struct Rect {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
}

impl Rect {
    pub fn contains(&self, px: usize, py: usize) -> bool {
        px >= self.x && px < self.x.saturating_add(self.w) && py >= self.y && py < self.y.saturating_add(self.h)
    }
}

#[derive(Copy, Clone)]
pub struct ContentArea {
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
}

#[derive(Copy, Clone)]
#[allow(dead_code)]
pub struct WindowMetrics {
    pub id: LayerId,
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
    pub char_w: usize,
    pub char_h: usize,
    pub content_area: Option<ContentArea>,
}

impl WindowMetrics {
    #[allow(dead_code)]
    pub fn content_rect_px(&self) -> Option<Rect> {
        let area = self.content_area?;
        let char_w = self.char_w.max(1);
        let char_h = self.char_h.max(1);
        let x = self.x.saturating_add(area.x.saturating_mul(char_w));
        let y = self.y.saturating_add(area.y.saturating_mul(char_h));
        let w = area.w.saturating_mul(char_w);
        let h = area.h.saturating_mul(char_h);
        if w == 0 || h == 0 {
            return None;
        }
        Some(Rect { x, y, w, h })
    }
}

#[derive(Copy, Clone)]
pub struct AppColors {
    pub fg: u32,
    pub bg: u32,
    pub scrollbar_bg: u32,
    pub scrollbar_thumb: u32,
}

pub struct AppContext {
    pub metrics: WindowMetrics,
    pub colors: AppColors,
}

impl AppContext {
    pub fn draw_text_at_char(&self, col: usize, row: usize, text: &str, fg: u32, bg: u32) {
        console::layer_draw_text_at_char(self.metrics.id, col, row, text, fg, bg);
    }

    #[allow(dead_code)]
    pub fn clear(&self, color: u32) {
        console::layer_clear(self.metrics.id, color);
    }

    pub fn fill_rect(&self, x: usize, y: usize, w: usize, h: usize, color: u32) {
        console::layer_fill_rect(self.metrics.id, x, y, w, h, color);
    }

    pub fn draw_wrapped_paragraphs(&self, area: ContentArea, paragraphs: &[&str], fg: u32, bg: u32) {
        let mut row = area.y;
        let max_row = area.y.saturating_add(area.h);
        for (idx, text) in paragraphs.iter().enumerate() {
            if row >= max_row {
                break;
            }
            row = self.draw_wrapped_text(area.x, row, area.w, max_row, text, fg, bg);
            if idx + 1 < paragraphs.len() {
                row = row.saturating_add(TEXT_GAP);
            }
        }
    }

    pub fn draw_wrapped_text(
        &self,
        x: usize,
        mut row: usize,
        width: usize,
        max_row: usize,
        text: &str,
        fg: u32,
        bg: u32,
    ) -> usize {
        let width = width.min(256);
        if width == 0 || row >= max_row {
            return row;
        }
        let mut line = HString::<256>::new();
        let mut line_len = 0usize;

        for word in text.split_whitespace() {
            let word_len = word.chars().count();
            if word_len == 0 {
                continue;
            }
            if word_len > width {
                if line_len > 0 {
                    self.draw_text_at_char(x, row, line.as_str(), fg, bg);
                    row = row.saturating_add(1);
                    line.clear();
                    line_len = 0;
                    if row >= max_row {
                        return row;
                    }
                }
                let mut buf = HString::<256>::new();
                let mut count = 0usize;
                for ch in word.chars() {
                    if count == width {
                        self.draw_text_at_char(x, row, buf.as_str(), fg, bg);
                        row = row.saturating_add(1);
                        if row >= max_row {
                            return row;
                        }
                        buf.clear();
                        count = 0;
                    }
                    let _ = buf.push(ch);
                    count += 1;
                }
                if count > 0 && row < max_row {
                    self.draw_text_at_char(x, row, buf.as_str(), fg, bg);
                    row = row.saturating_add(1);
                }
                if row >= max_row {
                    return row;
                }
                continue;
            }

            let additional = if line_len == 0 { word_len } else { word_len + 1 };
            if line_len + additional <= width {
                if line_len > 0 {
                    let _ = line.push(' ');
                }
                let _ = line.push_str(word);
                line_len += additional;
            } else {
                if line_len > 0 {
                    self.draw_text_at_char(x, row, line.as_str(), fg, bg);
                    row = row.saturating_add(1);
                    if row >= max_row {
                        return row;
                    }
                    line.clear();
                }
                let _ = line.push_str(word);
                line_len = word_len;
            }
        }

        if line_len > 0 && row < max_row {
            self.draw_text_at_char(x, row, line.as_str(), fg, bg);
            row = row.saturating_add(1);
        }
        row
    }

    pub fn draw_text_cursor(&self, col: usize, row: usize, color: u32) {
        let char_w = self.metrics.char_w.max(1);
        let char_h = self.metrics.char_h.max(1);
        if char_w == 0 || char_h == 0 {
            return;
        }
        let thickness = (char_h / 8).max(1).min(3);
        let x = col.saturating_mul(char_w);
        let y = row
            .saturating_mul(char_h)
            .saturating_add(char_h.saturating_sub(thickness));
        self.fill_rect(x, y, char_w, thickness, color);
    }

    pub fn draw_scrollbar(&self, track: Rect, thumb: Rect) {
        draw_scrollbar(self.metrics.id, track, thumb, self.colors.scrollbar_bg, self.colors.scrollbar_thumb);
    }
}

#[derive(Copy, Clone)]
pub struct ScrollbarDraw {
    pub track: Rect,
    pub thumb: Rect,
}

#[derive(Copy, Clone)]
pub struct ScrollMetrics {
    pub track: Rect,
    pub thumb_h: usize,
    pub max_scroll: usize,
    pub scroll: usize,
}

#[derive(Copy, Clone)]
pub enum AppEventResult {
    Ignored,
    HandledNoRedraw,
    HandledRedraw,
}

impl AppEventResult {
    pub fn handled(&self) -> bool {
        !matches!(*self, AppEventResult::Ignored)
    }

    pub fn needs_redraw(&self) -> bool {
        matches!(*self, AppEventResult::HandledRedraw)
    }
}

#[derive(Clone)]
pub enum AppAction {
    OpenFile { app_idx: usize, path: String },
}

#[derive(Copy, Clone)]
pub enum MouseEventKind {
    Down,
    Up,
}

#[derive(Copy, Clone)]
pub struct MouseEvent {
    pub col: usize,
    pub row: usize,
    pub kind: MouseEventKind,
    pub clicks: u8,
}

pub trait WindowApp {
    fn accepts_input(&self) -> bool {
        false
    }

    fn draw(&mut self, ctx: &mut AppContext, input_focus: bool);

    fn handle_key(&mut self, _ctx: &mut AppContext, _evt: &KeyEvent) -> AppEventResult {
        AppEventResult::Ignored
    }

    fn handle_mouse(&mut self, _ctx: &mut AppContext, _evt: &MouseEvent) -> AppEventResult {
        AppEventResult::Ignored
    }

    fn scroll_by(&mut self, _ctx: &mut AppContext, _lines: i32) -> bool {
        false
    }

    fn scroll_to(&mut self, _ctx: &mut AppContext, _scroll: usize) -> bool {
        false
    }

    fn scroll_metrics(&self, _ctx: &AppContext) -> Option<ScrollMetrics> {
        None
    }

    fn on_resize(&mut self, _ctx: &AppContext) {}

    fn take_action(&mut self) -> Option<AppAction> {
        None
    }

    fn open_path(&mut self, _path: &str) -> bool {
        false
    }
}

pub struct AppDescriptor {
    pub label: &'static str,
    pub default_title: &'static str,
    pub start_menu: bool,
    pub startup: bool,
    pub openable: bool,
    pub factory: fn() -> Box<dyn WindowApp>,
}

pub fn draw_scrollbar(id: LayerId, track: Rect, thumb: Rect, bg: u32, thumb_color: u32) {
    if track.w == 0 || track.h == 0 {
        return;
    }
    console::layer_fill_rect(id, track.x, track.y, track.w, track.h, bg);
    if thumb.w == 0 || thumb.h == 0 {
        return;
    }
    console::layer_fill_rect(id, thumb.x, thumb.y, thumb.w, thumb.h, thumb_color);
}

pub fn apply_intensity(color: u32, base: u32, intensity: u8) -> u32 {
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

const TEXT_GAP: usize = 1;
