use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;
use heapless::String as HString;

use crate::clipboard;
use crate::commands;
use crate::console::{self, CompositorMode, CursorBlink, CursorStyle, LayerId};
use crate::debug;
use crate::fs;
use crate::history;
use crate::keyboard::{KeyEvent, Keyboard};
use crate::mouse;
use crate::serial;
use crate::terminal;
use crate::{timer, time};

const DESKTOP_BG: u32 = 0x102028;
const WINDOW_BG: u32 = 0x1B1E23;
const WINDOW_FG: u32 = 0xE7E2D2;
const WINDOW_TITLE_ACTIVE: u32 = 0x1F667B;
const WINDOW_BORDER_ACTIVE: u32 = 0x59B4C9;
const WINDOW_TITLE_INACTIVE: u32 = 0x31363B;
const WINDOW_BORDER_INACTIVE: u32 = 0x3A3F44;
const WINDOW_TITLE_MOVE: u32 = 0x6B4C1E;
const WINDOW_BORDER_MOVE: u32 = 0xD19C3A;
const WINDOW_TITLE_RESIZE: u32 = 0x1E6B3E;
const WINDOW_BORDER_RESIZE: u32 = 0x4EC98A;
const TASKBAR_BG: u32 = 0x0E1418;
const TASKBAR_FG: u32 = 0xDCE6EA;
const TASKBAR_ACCENT: u32 = 0x263845;
const TASKBAR_ITEM_BG: u32 = 0x121A20;
const TASKBAR_ITEM_ACTIVE_BG: u32 = 0x1B2A35;
const START_MENU_BG: u32 = 0x10161B;
const START_MENU_BORDER: u32 = 0x2B3C47;
const SCROLLBAR_BG: u32 = 0x0F151A;
const SCROLLBAR_THUMB: u32 = 0x355A6A;
const CURSOR_FILL: u32 = 0xF8F8F8;
const CURSOR_BORDER: u32 = 0x0C0C0C;
const CURSOR_FILL_ACTIVE: u32 = 0x7FC8FF;
const CURSOR_BORDER_ACTIVE: u32 = 0x0C1C28;

const WINDOW_Z_START: i16 = 50;
const WINDOW_Z_STEP: i16 = 5;
const TASKBAR_Z: i16 = 900;
const CURSOR_Z: i16 = 30000;
const START_MENU_Z: i16 = 950;

const MIN_WINDOW_COLS: usize = 18;
const MIN_WINDOW_ROWS: usize = 6;
const BORDER_THICKNESS: usize = 2;
const WINDOW_PAD_X: usize = 2;
const WINDOW_PAD_Y: usize = 1;
const WINDOW_TEXT_GAP: usize = 1;
const NOTES_MAX_LINES: usize = 64;
const NOTES_MAX_COLS: usize = 128;
const SCROLL_LINES_PER_NOTCH: i32 = 3;
const SCROLLBAR_COLS: usize = 1;
const SCROLLBAR_GAP_COLS: usize = 1;
const CURSOR_HOT_X: usize = 2;
const CURSOR_HOT_Y: usize = 2;

#[derive(Copy, Clone, PartialEq, Eq)]
enum FocusInput {
    Auto,
    Clear,
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum WindowKind {
    Welcome,
    Notes,
    Help,
    Terminal,
}

#[derive(Clone)]
struct NotesBuffer {
    lines: Vec<HString<128>>,
    cursor_row: usize,
    scroll: usize,
    selection_all: bool,
}

impl NotesBuffer {
    fn new() -> Self {
        let mut lines = Vec::new();
        lines.push(HString::<128>::new());
        Self { lines, cursor_row: 0, scroll: 0, selection_all: false }
    }

    fn current_line_mut(&mut self) -> &mut HString<128> {
        if self.lines.is_empty() {
            self.lines.push(HString::<128>::new());
            self.cursor_row = 0;
        }
        if self.cursor_row >= self.lines.len() {
            self.cursor_row = self.lines.len().saturating_sub(1);
        }
        &mut self.lines[self.cursor_row]
    }

    fn push_char(&mut self, ch: char) -> bool {
        self.selection_all = false;
        let line = self.current_line_mut();
        if line.len() >= NOTES_MAX_COLS {
            return false;
        }
        line.push(ch).is_ok()
    }

    fn backspace(&mut self) -> bool {
        self.selection_all = false;
        let line = self.current_line_mut();
        if line.pop().is_some() {
            return true;
        }
        if self.cursor_row > 0 {
            self.lines.remove(self.cursor_row);
            self.cursor_row = self.cursor_row.saturating_sub(1);
            return true;
        }
        false
    }

    fn newline(&mut self) -> bool {
        self.selection_all = false;
        if self.lines.len() >= NOTES_MAX_LINES {
            return false;
        }
        let insert_at = self.cursor_row + 1;
        self.lines.insert(insert_at, HString::<128>::new());
        self.cursor_row = insert_at;
        true
    }

    fn select_all(&mut self) {
        self.selection_all = true;
    }

    fn clear_all(&mut self) -> bool {
        let changed = self.lines.len() != 1 || !self.lines.first().map(|line| line.is_empty()).unwrap_or(true);
        self.lines.clear();
        self.lines.push(HString::<128>::new());
        self.cursor_row = 0;
        self.scroll = 0;
        self.selection_all = false;
        changed
    }

    fn selection_text(&self) -> Option<String> {
        if !self.selection_all {
            return None;
        }
        let mut out = String::new();
        for (idx, line) in self.lines.iter().enumerate() {
            if idx > 0 {
                out.push('\n');
            }
            out.push_str(line.as_str());
        }
        Some(out)
    }

    fn insert_text(&mut self, text: &str) -> bool {
        self.selection_all = false;
        let mut changed = false;
        for ch in text.chars() {
            match ch {
                '\r' => {}
                '\n' => {
                    if self.newline() {
                        changed = true;
                    }
                }
                _ => {
                    if self.push_char(ch) {
                        changed = true;
                    }
                }
            }
        }
        changed
    }

    fn max_scroll(&self, view_rows: usize) -> usize {
        if view_rows == 0 {
            return 0;
        }
        self.lines.len().saturating_sub(view_rows)
    }

    fn clamp_scroll(&mut self, view_rows: usize) {
        let max_scroll = self.max_scroll(view_rows);
        if self.scroll > max_scroll {
            self.scroll = max_scroll;
        }
    }

    fn ensure_cursor_visible(&mut self, view_rows: usize) {
        if view_rows == 0 {
            return;
        }
        self.clamp_scroll(view_rows);
        if self.cursor_row < self.scroll {
            self.scroll = self.cursor_row;
        } else if self.cursor_row >= self.scroll.saturating_add(view_rows) {
            self.scroll = self.cursor_row.saturating_sub(view_rows - 1);
        }
    }

    fn scroll_by(&mut self, delta: i32, view_rows: usize) -> bool {
        if view_rows == 0 {
            return false;
        }
        let max_scroll = self.max_scroll(view_rows) as i32;
        let mut new_scroll = self.scroll as i32 + delta;
        if new_scroll < 0 {
            new_scroll = 0;
        }
        if new_scroll > max_scroll {
            new_scroll = max_scroll;
        }
        let new_scroll = new_scroll as usize;
        if new_scroll == self.scroll {
            return false;
        }
        self.scroll = new_scroll;
        true
    }
}

#[derive(Copy, Clone)]
struct Rect {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}

impl Rect {
    fn contains(&self, px: usize, py: usize) -> bool {
        px >= self.x && px < self.x.saturating_add(self.w) && py >= self.y && py < self.y.saturating_add(self.h)
    }
}

#[derive(Copy, Clone)]
struct ContentArea {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
}

struct NotesLayout {
    area: ContentArea,
    view_rows: usize,
    text_cols: usize,
    scrollbar_col: Option<usize>,
}

struct TerminalLayout {
    area: ContentArea,
    output_rows: usize,
    text_cols: usize,
    scrollbar_col: Option<usize>,
}

struct TaskbarItem {
    window: usize,
    rect: Rect,
}

struct ScrollbarDraw {
    track: Rect,
    thumb: Rect,
}

struct ScrollMetrics {
    track: Rect,
    thumb_h: usize,
    max_scroll: usize,
}

struct ScrollbarInfo {
    target: ScrollTarget,
    track: Rect,
    thumb: Rect,
}

#[derive(Copy, Clone)]
enum StartAction {
    Notes,
    Help,
    Terminal,
}

const START_MENU_ITEMS: &[(&str, StartAction)] = &[
    ("Notes", StartAction::Notes),
    ("Help", StartAction::Help),
    ("Terminal", StartAction::Terminal),
];

const WELCOME_TEXT: &[&str] = &[
    "Drag the title bar to move a window. Drag the corner to resize it.",
    "Alt+Tab switches windows. The Start key opens the menu.",
    "As the scroll wheel hasn't been perfected yet, simply use the scrollbar to navigate apps where applicable.",
];

const TERMINAL_PROMPT: &str = "> ";

#[derive(Clone)]
struct Window {
    id: LayerId,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    title: HString<32>,
    z: i16,
    kind: WindowKind,
    minimized: bool,
    notes: Option<NotesBuffer>,
}

struct Desktop {
    windows: Vec<Window>,
    focused: Option<usize>,
    next_z: i16,
    screen_w: usize,
    screen_h: usize,
    work_h: usize,
    char_w: usize,
    char_h: usize,
    taskbar_y: usize,
    taskbar_h: usize,
    taskbar: Option<LayerId>,
    cursor: Option<LayerId>,
    cursor_size: usize,
    cursor_x: usize,
    cursor_y: usize,
    cursor_buttons: u8,
    drag: Option<DragState>,
    scroll_drag: Option<ScrollDrag>,
    input_focus: Option<usize>,
    taskbar_items: Vec<TaskbarItem>,
    start_button: Option<Rect>,
    start_menu: Option<LayerId>,
    start_menu_rect: Option<Rect>,
    start_open: bool,
    start_menu_scroll: usize,
}

struct DragState {
    window: usize,
    grab_x: i32,
    grab_y: i32,
    mode: DragMode,
}

#[derive(Copy, Clone)]
enum DragMode {
    Move,
    Resize {
        start_w: usize,
        start_h: usize,
        start_mx: i32,
        start_my: i32,
    },
}

#[derive(Copy, Clone)]
enum ScrollTarget {
    Notes(usize),
    Terminal(usize),
    StartMenu,
}

#[derive(Copy, Clone)]
struct ScrollDrag {
    target: ScrollTarget,
    grab_offset: i32,
}

pub fn run() -> ! {
    let mut desktop = Desktop::new();
    let mut keyboard = Keyboard::new();
    let mut last_bar_tick = timer::ticks();

    loop {
        let mut needs_present = false;
        let _ = mouse::poll();
        if desktop.handle_mouse() {
            needs_present = true;
        }
        if let Some(evt) = keyboard.poll_event() {
            if desktop.handle_event(evt) {
                needs_present = true;
            }
        }
        if desktop.update_cursor() {
            needs_present = true;
        }
        if desktop.poll_time(&mut last_bar_tick) {
            needs_present = true;
        }
        if needs_present {
            console::present();
        }
        unsafe { x86::halt(); }
    }
}

impl Desktop {
    fn content_area(&self, win: &Window) -> Option<ContentArea> {
        let char_w = self.char_w.max(1);
        let char_h = self.char_h.max(1);
        let max_cols = win.w / char_w;
        let max_rows = win.h / char_h;
        if max_cols == 0 || max_rows == 0 {
            return None;
        }
        let title_rows = (self.title_bar_height().saturating_add(char_h - 1)) / char_h;
        let start_row = title_rows.saturating_add(WINDOW_PAD_Y);
        let cols = max_cols.saturating_sub(WINDOW_PAD_X * 2);
        let rows = max_rows.saturating_sub(start_row + WINDOW_PAD_Y);
        if cols == 0 || rows == 0 {
            return None;
        }
        Some(ContentArea {
            x: WINDOW_PAD_X,
            y: start_row,
            w: cols,
            h: rows,
        })
    }

    fn content_rect_px(&self, win: &Window) -> Option<Rect> {
        let area = self.content_area(win)?;
        let char_w = self.char_w.max(1);
        let char_h = self.char_h.max(1);
        let x = win.x.saturating_add(area.x.saturating_mul(char_w));
        let y = win.y.saturating_add(area.y.saturating_mul(char_h));
        let w = area.w.saturating_mul(char_w);
        let h = area.h.saturating_mul(char_h);
        if w == 0 || h == 0 {
            return None;
        }
        Some(Rect { x, y, w, h })
    }

    fn new() -> Self {
        let stats = console::display_buffer_stats().expect("Display unavailable");
        let (cols, rows) = console::size_chars();
        let char_w = (stats.width_px / cols.max(1)).max(1);
        let char_h = (stats.height_px / rows.max(1)).max(1);

        console::set_compositor_mode(CompositorMode::Layered);
        console::set_cursor_style(CursorStyle::Hidden);
        console::set_cursor_blink(CursorBlink::None);
        console::set_default_colors(TASKBAR_FG, DESKTOP_BG);
        console::clear_screen();

        let taskbar_h = char_h.saturating_add(6).max(char_h);
        let work_h = stats.height_px.saturating_sub(taskbar_h);
        let taskbar = console::create_layer(stats.width_px, taskbar_h, 0, work_h, TASKBAR_Z, 255);
        if let Some(id) = taskbar {
            console::layer_clear(id, TASKBAR_BG);
            console::layer_fill_rect(id, 0, 0, stats.width_px, 2, TASKBAR_ACCENT);
        } else {
            serial::write("desktop: taskbar layer alloc failed");
        }

        let cursor_size = char_w.saturating_add(char_w / 2).max(10).min(18);
        mouse::set_bounds(stats.width_px, stats.height_px);
        mouse::set_position(stats.width_px / 2, stats.height_px / 2);
        let cursor = console::create_layer(cursor_size, cursor_size, 0, 0, CURSOR_Z, 255);
        if let Some(id) = cursor {
            draw_cursor_layer(id, cursor_size, false);
        } else {
            serial::write("desktop: cursor layer alloc failed");
        }

        let mut desktop = Self {
            windows: Vec::new(),
            focused: None,
            next_z: WINDOW_Z_START,
            screen_w: stats.width_px,
            screen_h: stats.height_px,
            work_h,
            char_w,
            char_h,
            taskbar_y: work_h,
            taskbar_h,
            taskbar,
            cursor,
            cursor_size,
            cursor_x: 0,
            cursor_y: 0,
            cursor_buttons: 0,
            drag: None,
            scroll_drag: None,
            input_focus: None,
            taskbar_items: Vec::new(),
            start_button: None,
            start_menu: None,
            start_menu_rect: None,
            start_open: false,
            start_menu_scroll: 0,
        };

        desktop.spawn_default_windows();
        if desktop.windows.is_empty() {
            serial::write("desktop: window layer alloc failed");
        }
        desktop.draw_taskbar();
        desktop.update_cursor();
        console::present();
        desktop
    }

    fn poll_time(&mut self, last: &mut u64) -> bool {
        let now = timer::ticks();
        if now.wrapping_sub(*last) >= timer::frequency() as u64 {
            *last = now;
            self.draw_taskbar();
            return true;
        }
        false
    }

    fn handle_event(&mut self, evt: KeyEvent) -> bool {
        if self.handle_text_input(&evt) {
            return true;
        }
        let mut updated = false;
        match evt {
            KeyEvent::Tab | KeyEvent::AltTab => {
                self.focus_next();
                updated = true;
            }
            KeyEvent::Start => {
                self.input_focus = None;
                updated = self.set_start_menu_open(!self.start_open) || updated;
            }
            _ => {}
        }
        if updated {
            self.draw_taskbar();
        }
        updated
    }

    fn handle_text_input(&mut self, evt: &KeyEvent) -> bool {
        let Some(idx) = self.input_focus else { return false; };
        let Some(kind) = self.windows.get(idx).map(|win| win.kind) else {
            self.input_focus = None;
            return false;
        };
        if self.windows.get(idx).map(|win| win.minimized).unwrap_or(true) {
            self.input_focus = None;
            return false;
        }

        match kind {
            WindowKind::Notes => {}
            WindowKind::Terminal => {
                return self.handle_terminal_input(idx, evt);
            }
            _ => return false,
        }

        let view_rows = {
            let Some(win) = self.windows.get(idx) else { return false; };
            let Some(notes) = win.notes.as_ref() else { return false; };
            self.notes_layout(win, notes.lines.len()).map(|layout| layout.view_rows).unwrap_or(0)
        };

        let Some(win) = self.windows.get_mut(idx) else { return false; };
        let Some(notes) = win.notes.as_mut() else { return false; };

        let mut handled = false;
        let mut changed = false;
        match evt {
            &KeyEvent::Char(ch) => {
                handled = true;
                changed = notes.push_char(ch) || changed;
            }
            &KeyEvent::Backspace => {
                handled = true;
                changed = notes.backspace() || changed;
            }
            &KeyEvent::Enter => {
                handled = true;
                changed = notes.newline() || changed;
            }
            &KeyEvent::Tab => {
                handled = true;
                for _ in 0..4 {
                    changed = notes.push_char(' ') || changed;
                }
            }
            KeyEvent::CtrlA => {
                handled = true;
                notes.select_all();
            }
            KeyEvent::CtrlC => {
                handled = true;
                if let Some(text) = notes.selection_text() {
                    clipboard::set_text(&text);
                }
            }
            KeyEvent::CtrlX => {
                handled = true;
                if let Some(text) = notes.selection_text() {
                    clipboard::set_text(&text);
                    changed = notes.clear_all() || changed;
                }
            }
            KeyEvent::CtrlV => {
                handled = true;
                let clip_text = clipboard::get_text();
                if !clip_text.is_empty() {
                    if notes.selection_all {
                        changed = notes.clear_all() || changed;
                    }
                    changed = notes.insert_text(&clip_text) || changed;
                }
            }
            _ => {}
        }

        if handled {
            if changed {
                notes.ensure_cursor_visible(view_rows);
                self.draw_window(idx, self.focused == Some(idx));
            }
            return true;
        }
        false
    }

    fn terminal_prompt_text(&self) -> HString<128> {
        let mut prompt = HString::<128>::new();
        if commands::is_prompt_path_enabled() {
            let path = fs::prompt_path();
            let _ = prompt.push_str(&path);
        }
        let _ = prompt.push_str(TERMINAL_PROMPT);
        prompt
    }

    fn handle_terminal_input(&mut self, idx: usize, evt: &KeyEvent) -> bool {
        let Some(win) = self.windows.get(idx) else { return false; };
        if win.minimized {
            return false;
        }
        let prompt = self.terminal_prompt_text();
        let mut handled = false;
        let mut changed = false;
        let mut entered: Option<HString<128>> = None;
        let mut output_before: Option<usize> = None;
        let mut output_after: Option<usize> = None;
        let mut output_changed = false;
        let mut scroll_before = 0usize;

        terminal::with_state_mut(|term| {
            let len = term.input.chars().count();
            if term.cursor_pos > len {
                term.cursor_pos = len;
            }

            match evt {
                &KeyEvent::Char(ch) => {
                    handled = true;
                    if let Some(anchor) = term.selection_anchor {
                        changed = delete_selection(&mut term.input, &mut term.cursor_pos, anchor) || changed;
                        term.selection_anchor = None;
                    }
                    if insert_char_at(&mut term.input, term.cursor_pos, ch) {
                        term.cursor_pos += 1;
                        changed = true;
                    }
                    term.history_index = None;
                }
                &KeyEvent::CtrlBackspace => {
                    handled = true;
                    if let Some(anchor) = term.selection_anchor {
                        changed = delete_selection(&mut term.input, &mut term.cursor_pos, anchor) || changed;
                        term.selection_anchor = None;
                    }
                    if delete_prev_word(&mut term.input, &mut term.cursor_pos) {
                        changed = true;
                    }
                    term.history_index = None;
                }
                &KeyEvent::Backspace => {
                    handled = true;
                    if let Some(anchor) = term.selection_anchor {
                        changed = delete_selection(&mut term.input, &mut term.cursor_pos, anchor) || changed;
                        term.selection_anchor = None;
                    } else if term.cursor_pos > 0 && remove_char_at(&mut term.input, term.cursor_pos - 1) {
                        term.cursor_pos = term.cursor_pos.saturating_sub(1);
                        changed = true;
                    }
                    term.history_index = None;
                }
                &KeyEvent::Delete => {
                    handled = true;
                    if let Some(anchor) = term.selection_anchor {
                        changed = delete_selection(&mut term.input, &mut term.cursor_pos, anchor) || changed;
                        term.selection_anchor = None;
                    } else if remove_char_at(&mut term.input, term.cursor_pos) {
                        changed = true;
                    }
                    term.history_index = None;
                }
                &KeyEvent::Left => {
                    handled = true;
                    if term.selection_anchor.is_some() {
                        term.selection_anchor = None;
                    }
                    if term.cursor_pos > 0 {
                        term.cursor_pos -= 1;
                    }
                    changed = true;
                }
                &KeyEvent::Right => {
                    handled = true;
                    if term.selection_anchor.is_some() {
                        term.selection_anchor = None;
                    }
                    let len = term.input.chars().count();
                    if term.cursor_pos < len {
                        term.cursor_pos += 1;
                    }
                    changed = true;
                }
                &KeyEvent::CtrlLeft => {
                    handled = true;
                    term.selection_anchor = None;
                    move_cursor_word_left(&term.input, &mut term.cursor_pos);
                    changed = true;
                }
                &KeyEvent::CtrlRight => {
                    handled = true;
                    term.selection_anchor = None;
                    move_cursor_word_right(&term.input, &mut term.cursor_pos);
                    changed = true;
                }
                &KeyEvent::ShiftLeft => {
                    handled = true;
                    if term.selection_anchor.is_none() {
                        term.selection_anchor = Some(term.cursor_pos);
                    }
                    if term.cursor_pos > 0 {
                        term.cursor_pos -= 1;
                    }
                    changed = true;
                }
                &KeyEvent::ShiftRight => {
                    handled = true;
                    if term.selection_anchor.is_none() {
                        term.selection_anchor = Some(term.cursor_pos);
                    }
                    let len = term.input.chars().count();
                    if term.cursor_pos < len {
                        term.cursor_pos += 1;
                    }
                    changed = true;
                }
                &KeyEvent::CtrlShiftLeft => {
                    handled = true;
                    if term.selection_anchor.is_none() {
                        term.selection_anchor = Some(term.cursor_pos);
                    }
                    move_cursor_word_left(&term.input, &mut term.cursor_pos);
                    changed = true;
                }
                &KeyEvent::CtrlShiftRight => {
                    handled = true;
                    if term.selection_anchor.is_none() {
                        term.selection_anchor = Some(term.cursor_pos);
                    }
                    move_cursor_word_right(&term.input, &mut term.cursor_pos);
                    changed = true;
                }
                &KeyEvent::CtrlA => {
                    handled = true;
                    term.selection_anchor = Some(0);
                    term.cursor_pos = term.input.chars().count();
                    changed = true;
                }
                &KeyEvent::CtrlC => {
                    handled = true;
                    if let Some(anchor) = term.selection_anchor {
                        let start = anchor.min(term.cursor_pos);
                        let end = anchor.max(term.cursor_pos);
                        let mut selected = HString::<128>::new();
                        for (i, ch) in term.input.chars().enumerate() {
                            if i >= start && i < end {
                                let _ = selected.push(ch);
                            }
                        }
                        if !selected.is_empty() {
                            clipboard::set_text(selected.as_str());
                        }
                    }
                }
                &KeyEvent::CtrlX => {
                    handled = true;
                    if let Some(anchor) = term.selection_anchor {
                        let start = anchor.min(term.cursor_pos);
                        let end = anchor.max(term.cursor_pos);
                        let mut selected = HString::<128>::new();
                        for (i, ch) in term.input.chars().enumerate() {
                            if i >= start && i < end {
                                let _ = selected.push(ch);
                            }
                        }
                        if !selected.is_empty() {
                            clipboard::set_text(selected.as_str());
                        }
                        changed = delete_selection(&mut term.input, &mut term.cursor_pos, anchor) || changed;
                        term.selection_anchor = None;
                    }
                }
                &KeyEvent::CtrlV => {
                    handled = true;
                    if let Some(anchor) = term.selection_anchor {
                        changed = delete_selection(&mut term.input, &mut term.cursor_pos, anchor) || changed;
                        term.selection_anchor = None;
                    }
                    let clip_text = clipboard::get_text();
                    if !clip_text.is_empty() {
                        for ch in clip_text.chars() {
                            if insert_char_at(&mut term.input, term.cursor_pos, ch) {
                                term.cursor_pos += 1;
                                changed = true;
                            } else {
                                break;
                            }
                        }
                    }
                    term.history_index = None;
                }
                &KeyEvent::Tab => {
                    handled = true;
                    if let Some(cmd) = commands::suggest_command(term.input.as_str()) {
                        term.input.clear();
                        let _ = term.input.push_str(cmd.as_str());
                        term.cursor_pos = term.input.chars().count();
                        term.selection_anchor = None;
                        term.history_index = None;
                        changed = true;
                    }
                }
                &KeyEvent::Up => {
                    handled = true;
                    term.selection_anchor = None;
                    let hist_len = history::len();
                    if hist_len == 0 {
                        return;
                    }
                    if term.history_index.is_none() {
                        term.draft_line = term.input.clone();
                    }
                    let new_idx = term
                        .history_index
                        .map(|i| i.saturating_sub(1))
                        .unwrap_or_else(|| hist_len.saturating_sub(1));
                    if let Some(new_line) = history::entry(new_idx) {
                        term.history_index = Some(new_idx);
                        term.input = new_line;
                        term.cursor_pos = term.input.chars().count();
                        changed = true;
                    } else {
                        term.history_index = None;
                    }
                }
                &KeyEvent::Down => {
                    handled = true;
                    term.selection_anchor = None;
                    let hist_len = history::len();
                    if hist_len == 0 {
                        return;
                    }
                    if let Some(idx) = term.history_index {
                        if idx + 1 < hist_len {
                            let new_idx = idx + 1;
                            if let Some(new_line) = history::entry(new_idx) {
                                term.history_index = Some(new_idx);
                                term.input = new_line;
                            } else {
                                term.history_index = None;
                                term.input = term.draft_line.clone();
                            }
                        } else {
                            term.history_index = None;
                            term.input = term.draft_line.clone();
                        }
                        term.cursor_pos = term.input.chars().count();
                        changed = true;
                    }
                }
                &KeyEvent::Enter => {
                    handled = true;
                    entered = Some(term.input.clone());
                    term.input.clear();
                    term.cursor_pos = 0;
                    term.selection_anchor = None;
                    term.history_index = None;
                    term.draft_line.clear();
                }
                _ => {}
            }
        });

        if let Some(line) = entered {
            let mut before_layout: Option<(usize, usize, bool)> = None;
            let mut before_lines: Option<usize> = None;
            if let Some((layout, total_lines)) = self.terminal_layout_and_lines(win) {
                before_layout = Some((layout.output_rows, layout.text_cols.min(128), layout.scrollbar_col.is_some()));
                before_lines = Some(total_lines);
                scroll_before = terminal::scroll();
            }
            let trimmed = line.as_str().trim();
            if !trimmed.is_empty() {
                let mut echoed = HString::<256>::new();
                let _ = echoed.push_str(prompt.as_str());
                let _ = echoed.push_str(line.as_str());
                terminal::push_output(echoed.as_str(), true);
                self.run_terminal_command(line.as_str());
                history::push(line.as_str());
            } else {
                terminal::push_output(prompt.as_str(), true);
            }
            let mut after_lines: Option<usize> = None;
            if let Some((layout, total_lines)) = self.terminal_layout_and_lines(win) {
                let after_layout = (layout.output_rows, layout.text_cols.min(128), layout.scrollbar_col.is_some());
                if Some(after_layout) == before_layout {
                    after_lines = Some(total_lines);
                }
            }
            if let (Some(before), Some(after)) = (before_lines, after_lines) {
                output_before = Some(before);
                output_after = Some(after);
            }
            output_changed = true;
            changed = true;
        }

        if handled {
            if changed {
                let did_redraw = if let (Some(before), Some(after)) = (output_before, output_after) {
                    self.redraw_terminal_output_delta(idx, before, after, scroll_before)
                } else if output_changed {
                    false
                } else {
                    self.redraw_terminal_input_row(idx)
                };
                if !did_redraw {
                    self.draw_window(idx, self.focused == Some(idx));
                }
            }
            return true;
        }
        false
    }

    fn run_terminal_command(&self, line: &str) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return;
        }
        if trimmed.eq_ignore_ascii_case("clear") || trimmed.eq_ignore_ascii_case("cls") {
            terminal::clear_output();
            return;
        }
        let mut parts = trimmed.split_whitespace();
        let Some(cmd) = parts.next() else { return; };
        if cmd.eq_ignore_ascii_case("cecho") {
            let Some(color) = parts.next() else {
                terminal::push_output("Usage: cecho <hex> <text>", true);
                return;
            };
            let fg = match parse_rgb_hex(color) {
                Some(v) if v <= 0xFFFFFF => v,
                _ => {
                    terminal::push_output("cecho: invalid hex. Use 3 or 6 hex digits, e.g., FF0000", true);
                    return;
                }
            };
            let mut text = HString::<128>::new();
            for (i, word) in parts.enumerate() {
                if i > 0 {
                    let _ = text.push(' ');
                }
                let _ = text.push_str(word);
            }
            if text.is_empty() {
                terminal::push_output("Usage: cecho <hex> <text>", true);
            } else {
                terminal::push_output_colored(text.as_str(), fg, true);
            }
            return;
        }
        if cmd.eq_ignore_ascii_case("os") {
            let allow_os = debug::terminal_os_commands_enabled();
            if let Some(sub) = parts.next() {
                if !allow_os
                    && (sub.eq_ignore_ascii_case("cursor")
                        || sub.eq_ignore_ascii_case("text")
                        || sub.eq_ignore_ascii_case("bg")
                        || sub.eq_ignore_ascii_case("hud")
                        || sub.eq_ignore_ascii_case("font"))
                {
                    terminal::push_output(commands::DESKTOP_PORTING_DISABLED_MSG, true);
                    return;
                }
            }
        }
        console::set_output_hook(Some(terminal::console_output_hook));
        commands::handle_line(line);
        console::set_output_hook(None);
    }

    fn handle_mouse(&mut self) -> bool {
        let (mx, my) = mouse::position();
        let buttons = mouse::buttons();
        let prev_buttons = self.cursor_buttons;
        let left_down = (buttons & 0x01) != 0;
        let left_prev = (prev_buttons & 0x01) != 0;
        let left_pressed = left_down && !left_prev;
        let left_released = !left_down && left_prev;
        let mut updated = false;
        let mut taskbar_dirty = false;

        let wheel = mouse::take_wheel_delta();
        if wheel != 0 {
            let mut target = self.scroll_target_at(mx, my);
            if target.is_none() {
                if let Some(idx) = self.window_at(mx, my) {
                    target = self.scroll_target_for_window(idx);
                } else if let Some(idx) = self.focused {
                    target = self.scroll_target_for_window(idx);
                }
            }
            if let Some(target) = target {
                let lines = -wheel * SCROLL_LINES_PER_NOTCH;
                if self.scroll_by_lines(target, lines) {
                    updated = true;
                }
            }
        }

        if let Some(drag) = self.scroll_drag {
            if left_down {
                if self.scroll_to_thumb(drag.target, my as i32, drag.grab_offset) {
                    updated = true;
                }
            }
            if left_released {
                self.scroll_drag = None;
            }
            if taskbar_dirty {
                self.draw_taskbar();
            }
            return updated;
        }

        if left_pressed {
            if let Some(hit) = self.scrollbar_hit_at(mx, my) {
                let grab_offset = if hit.thumb.contains(mx, my) {
                    my as i32 - hit.thumb.y as i32
                } else {
                    (hit.thumb.h / 2) as i32
                };
                if self.scroll_to_thumb(hit.target, my as i32, grab_offset) {
                    updated = true;
                }
                self.scroll_drag = Some(ScrollDrag { target: hit.target, grab_offset });
                return updated;
            }

            if let Some(action) = self.start_menu_action_at(mx, my) {
                self.set_start_menu_open(false);
                self.handle_start_action(action);
                taskbar_dirty = true;
                self.drag = None;
                if taskbar_dirty {
                    self.draw_taskbar();
                }
                return true;
            }

            if self.is_in_taskbar(my) {
                if self.handle_taskbar_click(mx, my) {
                    taskbar_dirty = true;
                    updated = true;
                } else if self.start_open {
                    if self.set_start_menu_open(false) {
                        taskbar_dirty = true;
                        updated = true;
                    }
                }
                self.drag = None;
                if taskbar_dirty {
                    self.draw_taskbar();
                }
                return updated;
            }

            if self.start_open && !self.start_menu_contains(mx, my) {
                if self.set_start_menu_open(false) {
                    taskbar_dirty = true;
                    updated = true;
                }
            }
        }

        if left_pressed {
            if let Some(idx) = self.window_at(mx, my) {
                if self.hit_close(idx, mx, my) {
                    self.close_window_idx(idx);
                    self.drag = None;
                    self.input_focus = None;
                    return true;
                }
                if self.hit_minimize(idx, mx, my) {
                    if self.set_window_minimized(idx, true) {
                        if let Some(next) = self.next_visible(idx) {
                            self.focus_window(next, FocusInput::Auto);
                        } else {
                            self.focused = None;
                            self.input_focus = None;
                        }
                        taskbar_dirty = true;
                    }
                    self.drag = None;
                    if taskbar_dirty {
                        self.draw_taskbar();
                    }
                    return true;
                }
                if self.hit_resize_corner(idx, mx, my) {
                    self.focus_window(idx, FocusInput::Clear);
                    taskbar_dirty = true;
                    if let Some(win) = self.windows.get(idx) {
                        self.drag = Some(DragState {
                            window: idx,
                            grab_x: 0,
                            grab_y: 0,
                            mode: DragMode::Resize {
                                start_w: win.w,
                                start_h: win.h,
                                start_mx: mx as i32,
                                start_my: my as i32,
                            },
                        });
                    }
                    if taskbar_dirty {
                        self.draw_taskbar();
                    }
                    return true;
                }
                let in_title = self.hit_title_bar(idx, my);
                let input = if in_title { FocusInput::Clear } else { FocusInput::Auto };
                self.focus_window(idx, input);
                updated = true;
                taskbar_dirty = true;
                if in_title {
                    if let Some(win) = self.windows.get(idx) {
                        self.drag = Some(DragState {
                            window: idx,
                            grab_x: mx as i32 - win.x as i32,
                            grab_y: my as i32 - win.y as i32,
                            mode: DragMode::Move,
                        });
                    }
                }
            } else {
                self.input_focus = None;
            }
        }

        if let Some(drag) = &self.drag {
            if left_down {
                let mode = drag.mode;
                match mode {
                    DragMode::Move => {
                        if let Some(win) = self.windows.get_mut(drag.window) {
                            let mut new_x = mx as i32 - drag.grab_x;
                            let mut new_y = my as i32 - drag.grab_y;
                            let max_x = self.screen_w.saturating_sub(win.w) as i32;
                            let max_y = self.work_h.saturating_sub(win.h) as i32;
                            if new_x < 0 { new_x = 0; }
                            if new_y < 0 { new_y = 0; }
                            if new_x > max_x { new_x = max_x; }
                            if new_y > max_y { new_y = max_y; }
                            let new_x = new_x as usize;
                            let new_y = new_y as usize;
                            if new_x != win.x || new_y != win.y {
                                win.x = new_x;
                                win.y = new_y;
                                console::layer_set_pos(win.id, new_x, new_y);
                                updated = true;
                            }
                        } else {
                            self.drag = None;
                        }
                    }
                    DragMode::Resize { start_w, start_h, start_mx, start_my } => {
                        let dx = mx as i32 - start_mx;
                        let dy = my as i32 - start_my;
                        let new_w = (start_w as i32 + dx).max(1);
                        let new_h = (start_h as i32 + dy).max(1);
                        if let Some(win) = self.windows.get(drag.window) {
                            let max_w = self.screen_w.saturating_sub(win.x).max(self.min_window_w());
                            let max_h = self.work_h.saturating_sub(win.y).max(self.min_window_h());
                            let new_w = clamp_dim(new_w, self.min_window_w(), max_w);
                            let new_h = clamp_dim(new_h, self.min_window_h(), max_h);
                            if new_w != win.w || new_h != win.h {
                                self.resize_window_to(drag.window, new_w, new_h);
                                updated = true;
                            }
                        } else {
                            self.drag = None;
                        }
                    }
                }
            } else if left_released {
                self.drag = None;
            }
        } else if left_released {
            self.drag = None;
        }

        if taskbar_dirty {
            self.draw_taskbar();
        }
        updated
    }

    fn spawn_default_windows(&mut self) {
        let pad = self.char_w.saturating_mul(2).max(16);
        let w1 = (self.screen_w * 4 / 5).min(self.screen_w.saturating_sub(pad * 2));
        let h1 = (self.work_h * 2 / 3).min(self.work_h.saturating_sub(pad * 2));
        let _ = self.create_window(pad, pad, w1.max(1), h1.max(1), "Welcome", WindowKind::Welcome);
    }

    fn spawn_notes_window(&mut self) {
        let title = self.next_app_title("Notes");
        self.spawn_app_window(WindowKind::Notes, title.as_str());
    }

    fn spawn_help_window(&mut self) {
        let title = self.next_app_title("Help");
        self.spawn_app_window(WindowKind::Help, title.as_str());
    }

    fn spawn_terminal_window(&mut self) {
        let title = self.next_app_title("Terminal");
        self.spawn_app_window(WindowKind::Terminal, title.as_str());
    }

    fn spawn_app_window(&mut self, kind: WindowKind, title: &str) {
        let count = self.windows.len() as i32;
        let offset = (count * 12).rem_euclid(80) as usize;
        let w = self.min_window_w().max(self.screen_w / 3);
        let h = self.min_window_h().max(self.work_h / 3);
        let x = (24 + offset).min(self.screen_w.saturating_sub(w));
        let y = (24 + offset).min(self.work_h.saturating_sub(h));
        let _ = self.create_window(x, y, w, h, title, kind);
    }

    fn next_app_title(&self, base: &str) -> HString<32> {
        let mut count = 0usize;
        for win in &self.windows {
            if win.title.as_str().starts_with(base) {
                count += 1;
            }
        }
        let mut title = HString::<32>::new();
        if count == 0 {
            let _ = title.push_str(base);
        } else {
            let _ = write!(&mut title, "{} {}", base, count + 1);
        }
        title
    }

    fn create_window(&mut self, x: usize, y: usize, w: usize, h: usize, title: &str, kind: WindowKind) -> Option<usize> {
        let w = w.min(self.screen_w.max(1)).max(1);
        let h = h.min(self.work_h.max(1)).max(1);
        let x = x.min(self.screen_w.saturating_sub(w));
        let y = y.min(self.work_h.saturating_sub(h));

        let z = self.next_z;
        self.next_z = self.next_z.saturating_add(WINDOW_Z_STEP);
        let id = console::create_layer(w, h, x, y, z, 255)?;

        let mut title_buf = HString::<32>::new();
        let _ = title_buf.push_str(title);
        let notes = if matches!(kind, WindowKind::Notes) {
            Some(NotesBuffer::new())
        } else {
            None
        };
        let win = Window { id, x, y, w, h, title: title_buf, z, kind, minimized: false, notes };
        self.windows.push(win);
        let idx = self.windows.len().saturating_sub(1);
        self.focus_window(idx, FocusInput::Auto);
        Some(idx)
    }

    fn focus_next(&mut self) {
        if self.windows.is_empty() {
            self.focused = None;
            self.input_focus = None;
            return;
        }
        let start = self.focused.unwrap_or(usize::MAX);
        if let Some(next) = self.next_visible(start) {
            self.focus_window(next, FocusInput::Auto);
        } else {
            self.focused = None;
            self.input_focus = None;
        }
    }

    fn next_visible(&self, start: usize) -> Option<usize> {
        let len = self.windows.len();
        if len == 0 {
            return None;
        }
        for offset in 1..=len {
            let idx = if start == usize::MAX {
                offset - 1
            } else {
                (start + offset) % len
            };
            if !self.windows[idx].minimized {
                return Some(idx);
            }
        }
        None
    }

    fn first_visible_from(&self, start: usize) -> Option<usize> {
        let len = self.windows.len();
        if len == 0 {
            return None;
        }
        let start = start.min(len - 1);
        for offset in 0..len {
            let idx = (start + offset) % len;
            if !self.windows[idx].minimized {
                return Some(idx);
            }
        }
        None
    }

    fn focus_window(&mut self, idx: usize, input: FocusInput) {
        if idx >= self.windows.len() {
            return;
        }
        if self.windows.get(idx).map(|win| win.minimized).unwrap_or(false) {
            let _ = self.set_window_minimized(idx, false);
        }
        let prev = self.focused;
        self.focused = Some(idx);
        match input {
            FocusInput::Auto => {
                if self.window_accepts_input(idx) {
                    self.input_focus = Some(idx);
                } else {
                    self.input_focus = None;
                }
            }
            FocusInput::Clear => {
                self.input_focus = None;
            }
        }
        self.raise_window(idx);
        if let Some(prev_idx) = prev {
            if prev_idx < self.windows.len() && prev_idx != idx {
                self.draw_window(prev_idx, false);
            }
        }
        self.draw_window(idx, true);
    }

    fn raise_window(&mut self, idx: usize) {
        let z = self.next_z;
        self.next_z = self.next_z.saturating_add(WINDOW_Z_STEP);
        if let Some(win) = self.windows.get_mut(idx) {
            win.z = z;
            console::layer_set_z(win.id, z);
        }
        if self.next_z > CURSOR_Z.saturating_sub(64) {
            self.normalize_z();
        }
    }

    fn normalize_z(&mut self) {
        let mut z = WINDOW_Z_START;
        for win in self.windows.iter_mut() {
            win.z = z;
            console::layer_set_z(win.id, z);
            z = z.saturating_add(WINDOW_Z_STEP);
        }
        self.next_z = z;
    }

    fn window_at(&self, x: usize, y: usize) -> Option<usize> {
        let mut best: Option<(usize, i16)> = None;
        for (idx, win) in self.windows.iter().enumerate() {
            if win.minimized {
                continue;
            }
            let within_x = x >= win.x && x < win.x.saturating_add(win.w);
            let within_y = y >= win.y && y < win.y.saturating_add(win.h);
            if within_x && within_y {
                match best {
                    Some((_, z)) if win.z <= z => {}
                    _ => best = Some((idx, win.z)),
                }
            }
        }
        best.map(|(idx, _)| idx)
    }

    fn window_accepts_input(&self, idx: usize) -> bool {
        self.windows
            .get(idx)
            .map(|win| matches!(win.kind, WindowKind::Notes | WindowKind::Terminal) && !win.minimized)
            .unwrap_or(false)
    }

    fn hit_title_bar(&self, idx: usize, y: usize) -> bool {
        let Some(win) = self.windows.get(idx) else { return false; };
        let title_h = self.title_bar_height().min(win.h);
        y >= win.y && y < win.y.saturating_add(title_h)
    }

    fn hit_close(&self, idx: usize, x: usize, y: usize) -> bool {
        let Some(win) = self.windows.get(idx) else { return false; };
        let char_w = self.char_w;
        let char_h = self.char_h;
        if char_w == 0 || char_h == 0 {
            return false;
        }
        let max_cols = win.w / char_w;
        if max_cols < 4 {
            return false;
        }
        let x_char = max_cols - 4;
        let x0 = win.x.saturating_add(x_char.saturating_mul(char_w));
        let y0 = win.y;
        let w = char_w.saturating_mul(3);
        let h = char_h;
        x >= x0 && x < x0.saturating_add(w) && y >= y0 && y < y0.saturating_add(h)
    }

    fn hit_minimize(&self, idx: usize, x: usize, y: usize) -> bool {
        let Some(win) = self.windows.get(idx) else { return false; };
        let char_w = self.char_w;
        let char_h = self.char_h;
        if char_w == 0 || char_h == 0 {
            return false;
        }
        let max_cols = win.w / char_w;
        if max_cols < 8 {
            return false;
        }
        let x_char = max_cols - 8;
        let x0 = win.x.saturating_add(x_char.saturating_mul(char_w));
        let y0 = win.y;
        let w = char_w.saturating_mul(3);
        let h = char_h;
        x >= x0 && x < x0.saturating_add(w) && y >= y0 && y < y0.saturating_add(h)
    }

    fn hit_resize_corner(&self, idx: usize, x: usize, y: usize) -> bool {
        let Some(win) = self.windows.get(idx) else { return false; };
        let size = self.char_w.max(self.char_h).saturating_add(4).min(24).max(10);
        let x0 = win.x.saturating_add(win.w.saturating_sub(size));
        let y0 = win.y.saturating_add(win.h.saturating_sub(size));
        x >= x0 && y >= y0
    }

    fn draw_window(&self, idx: usize, active: bool) {
        let Some(win) = self.windows.get(idx) else { return; };
        if win.minimized {
            return;
        }
        let (title_color, border_color) = if active {
            match self.drag.as_ref().filter(|drag| drag.window == idx).map(|drag| drag.mode) {
                Some(DragMode::Move) => (WINDOW_TITLE_MOVE, WINDOW_BORDER_MOVE),
                Some(DragMode::Resize { .. }) => (WINDOW_TITLE_RESIZE, WINDOW_BORDER_RESIZE),
                None => (WINDOW_TITLE_ACTIVE, WINDOW_BORDER_ACTIVE),
            }
        } else {
            (WINDOW_TITLE_INACTIVE, WINDOW_BORDER_INACTIVE)
        };

        console::layer_clear(win.id, WINDOW_BG);

        let title_h = self.title_bar_height().min(win.h);
        console::layer_fill_rect(win.id, 0, 0, win.w, title_h, title_color);

        let border = BORDER_THICKNESS.min(win.w).min(win.h);
        if border > 0 {
            console::layer_fill_rect(win.id, 0, 0, win.w, border, border_color);
            if win.h > border {
                console::layer_fill_rect(win.id, 0, win.h - border, win.w, border, border_color);
            }
            console::layer_fill_rect(win.id, 0, 0, border, win.h, border_color);
            if win.w > border {
                console::layer_fill_rect(win.id, win.w - border, 0, border, win.h, border_color);
            }
        }

        if self.char_w > 0 && self.char_h > 0 {
            let input_focus = self.input_focus == Some(idx);
            console::layer_draw_text_at_char(win.id, 1, 0, win.title.as_str(), WINDOW_FG, title_color);
            self.draw_window_controls(win, title_color);
            self.draw_window_body(win, input_focus);
        }
    }

    fn draw_window_controls(&self, win: &Window, title_color: u32) {
        let max_cols = win.w / self.char_w;
        if max_cols >= 8 {
            let x = max_cols.saturating_sub(8);
            console::layer_draw_text_at_char(win.id, x, 0, "[_]", WINDOW_FG, title_color);
        }
        if max_cols >= 4 {
            let x = max_cols.saturating_sub(4);
            console::layer_draw_text_at_char(win.id, x, 0, "[x]", WINDOW_FG, title_color);
        }
    }

    fn draw_window_body(&self, win: &Window, input_focus: bool) {
        match win.kind {
            WindowKind::Welcome => {
                self.draw_info_window(win, WELCOME_TEXT);
            }
            WindowKind::Notes => {
                self.draw_notes(win, input_focus);
            }
            WindowKind::Help => {
                self.draw_info_window(win, WELCOME_TEXT);
            }
            WindowKind::Terminal => {
                self.draw_terminal(win, input_focus);
            }
        }
    }

    fn draw_info_window(&self, win: &Window, paragraphs: &[&str]) {
        let Some(area) = self.content_area(win) else { return; };
        self.draw_wrapped_paragraphs(win.id, area, paragraphs, WINDOW_FG, WINDOW_BG);
    }

    fn draw_wrapped_paragraphs(&self, id: LayerId, area: ContentArea, paragraphs: &[&str], fg: u32, bg: u32) {
        let mut row = area.y;
        let max_row = area.y.saturating_add(area.h);
        for (idx, text) in paragraphs.iter().enumerate() {
            if row >= max_row {
                break;
            }
            row = self.draw_wrapped_text(id, area.x, row, area.w, max_row, text, fg, bg);
            if idx + 1 < paragraphs.len() {
                row = row.saturating_add(WINDOW_TEXT_GAP);
            }
        }
    }

    fn draw_wrapped_text(
        &self,
        id: LayerId,
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
                    console::layer_draw_text_at_char(id, x, row, line.as_str(), fg, bg);
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
                        console::layer_draw_text_at_char(id, x, row, buf.as_str(), fg, bg);
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
                    console::layer_draw_text_at_char(id, x, row, buf.as_str(), fg, bg);
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
                    console::layer_draw_text_at_char(id, x, row, line.as_str(), fg, bg);
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
            console::layer_draw_text_at_char(id, x, row, line.as_str(), fg, bg);
            row = row.saturating_add(1);
        }
        row
    }

    fn draw_notes(&self, win: &Window, input_focus: bool) {
        let Some(notes) = win.notes.as_ref() else { return; };
        let total_lines = notes.lines.len();
        let layout = self.notes_layout(win, total_lines);
        let Some(layout) = layout else { return; };
        let view_rows = layout.view_rows;
        let text_cols = layout.text_cols;
        let start_row = layout.area.y;
        let start_col = layout.area.x;
        let max_start = total_lines.saturating_sub(view_rows);
        let start_line = notes.scroll.min(max_start);
        for (i, line) in notes.lines.iter().skip(start_line).take(view_rows).enumerate() {
            if text_cols == 0 {
                continue;
            }
            let mut buf = HString::<128>::new();
            for ch in line.chars().take(text_cols) {
                let _ = buf.push(ch);
            }
            console::layer_draw_text_at_char(win.id, start_col, start_row + i, buf.as_str(), WINDOW_FG, WINDOW_BG);
        }
        if input_focus && total_lines > 0 {
            let cursor_row = notes.cursor_row.min(total_lines.saturating_sub(1));
            if cursor_row >= start_line {
                let row = start_row + cursor_row - start_line;
                if row < start_row.saturating_add(view_rows) {
                    let line = &notes.lines[cursor_row];
                    let col = line.len().min(text_cols.saturating_sub(1));
                    if text_cols > 0 {
                        self.draw_text_cursor(win.id, start_col + col, row, WINDOW_FG);
                    }
                }
            }
        }
        if let Some(scrollbar) = self.notes_scrollbar_draw(win, &layout, view_rows, total_lines) {
            self.draw_scrollbar(win.id, scrollbar.track, scrollbar.thumb);
        }
    }

    fn draw_terminal(&self, win: &Window, input_focus: bool) {
        let Some((layout, total_lines)) = self.terminal_layout_and_lines(win) else { return; };
        let output_rows = layout.output_rows;
        let text_cols = layout.text_cols.min(128);
        if text_cols == 0 {
            return;
        }
        let scroll = terminal::scroll();
        let start_row = layout.area.y;
        let start_col = layout.area.x;

        terminal::with_state(|term| {
            self.draw_terminal_output_range(
                win.id,
                start_col,
                start_row,
                text_cols,
                output_rows,
                scroll,
                term,
            );
            self.draw_terminal_input_row(win, &layout, input_focus, term);
        });

        if let Some(scrollbar) = self.terminal_scrollbar_draw(&layout, total_lines) {
            self.draw_scrollbar(win.id, scrollbar.track, scrollbar.thumb);
        }
    }

    fn draw_terminal_input_row(
        &self,
        win: &Window,
        layout: &TerminalLayout,
        input_focus: bool,
        term: &terminal::TerminalState,
    ) {
        let text_cols = layout.text_cols.min(128);
        if text_cols == 0 {
            return;
        }
        let prompt = self.terminal_prompt_text();
        let prompt_len = prompt.chars().count().min(text_cols);
        let input_row = layout.area.y.saturating_add(layout.output_rows);
        if input_row >= layout.area.y.saturating_add(layout.area.h) {
            return;
        }
        let start_col = layout.area.x;
        let mut input_buf = HString::<128>::new();
        for ch in prompt.chars().take(text_cols) {
            let _ = input_buf.push(ch);
        }
        let used = prompt_len;
        let remaining = text_cols.saturating_sub(used);
        for ch in term.input.chars().take(remaining) {
            let _ = input_buf.push(ch);
        }
        console::layer_draw_text_at_char(win.id, start_col, input_row, input_buf.as_str(), WINDOW_FG, WINDOW_BG);
        if input_focus {
            let suggestion = commands::suggest_command(term.input.as_str());
            let suggestion_suffix = suggestion
                .as_ref()
                .and_then(|s| s.as_str().get(term.input.as_str().len()..));
            if let Some(suffix) = suggestion_suffix {
                let input_len = term.input.chars().count();
                let remaining_cols = text_cols
                    .saturating_sub(prompt_len)
                    .saturating_sub(input_len);
                if remaining_cols > 0 {
                    let mut ghost = HString::<128>::new();
                    for ch in suffix.chars().take(remaining_cols) {
                        let _ = ghost.push(ch);
                    }
                    if !ghost.is_empty() {
                        let ghost_color = apply_intensity(WINDOW_FG, WINDOW_BG, 100);
                        let ghost_x = start_col
                            .saturating_add(prompt_len)
                            .saturating_add(input_len);
                        console::layer_draw_text_at_char(win.id, ghost_x, input_row, ghost.as_str(), ghost_color, WINDOW_BG);
                    }
                }
            }
        }
        if input_focus && text_cols > 0 {
            let cursor_pos = term.cursor_pos.min(term.input.chars().count());
            let cursor_col = prompt_len
                .saturating_add(cursor_pos)
                .min(text_cols.saturating_sub(1));
            self.draw_text_cursor(win.id, start_col + cursor_col, input_row, WINDOW_FG);
        }
    }

    fn draw_terminal_output_range(
        &self,
        id: LayerId,
        start_col: usize,
        start_row: usize,
        text_cols: usize,
        max_rows: usize,
        skip_rows: usize,
        term: &terminal::TerminalState,
    ) {
        if text_cols == 0 || max_rows == 0 {
            return;
        }
        let mut remaining_skip = skip_rows;
        let mut row = 0usize;
        'lines: for line in &term.lines {
            let fg = line.fg.unwrap_or(WINDOW_FG);
            let bg = line.bg.unwrap_or(WINDOW_BG);
            if line.text.is_empty() {
                if remaining_skip > 0 {
                    remaining_skip -= 1;
                } else {
                    row = row.saturating_add(1);
                }
                if row >= max_rows {
                    break 'lines;
                }
                continue;
            }
            let mut buf = HString::<128>::new();
            let mut count = 0usize;
            for ch in line.text.chars() {
                if count == text_cols {
                    if remaining_skip > 0 {
                        remaining_skip -= 1;
                    } else {
                        console::layer_draw_text_at_char(id, start_col, start_row + row, buf.as_str(), fg, bg);
                        row = row.saturating_add(1);
                        if row >= max_rows {
                            break 'lines;
                        }
                    }
                    buf.clear();
                    count = 0;
                }
                let _ = buf.push(ch);
                count += 1;
            }
            if count > 0 {
                if remaining_skip > 0 {
                    remaining_skip -= 1;
                } else {
                    console::layer_draw_text_at_char(id, start_col, start_row + row, buf.as_str(), fg, bg);
                    row = row.saturating_add(1);
                    if row >= max_rows {
                        break 'lines;
                    }
                }
            }
        }
    }

    fn redraw_terminal_input_row(&self, idx: usize) -> bool {
        let Some(win) = self.windows.get(idx) else { return false; };
        if win.minimized {
            return false;
        }
        let Some((layout, _)) = self.terminal_layout_and_lines(win) else { return false; };
        let char_w = self.char_w.max(1);
        let char_h = self.char_h.max(1);
        let input_row = layout.area.y.saturating_add(layout.output_rows);
        let x_px = layout.area.x.saturating_mul(char_w);
        let y_px = input_row.saturating_mul(char_h);
        let w_px = layout.area.w.saturating_mul(char_w);
        let h_px = char_h;
        console::layer_fill_rect(win.id, x_px, y_px, w_px, h_px, WINDOW_BG);
        let input_focus = self.input_focus == Some(idx);
        terminal::with_state(|term| {
            self.draw_terminal_input_row(win, &layout, input_focus, term);
        });
        true
    }

    fn redraw_terminal_output_delta(
        &self,
        idx: usize,
        before_lines: usize,
        _after_lines: usize,
        scroll_before: usize,
    ) -> bool {
        let Some(win) = self.windows.get(idx) else { return false; };
        if win.minimized {
            return false;
        }
        let Some((layout, current_lines)) = self.terminal_layout_and_lines(win) else { return false; };
        let after_lines = current_lines;
        if after_lines <= before_lines {
            return false;
        }
        let output_rows = layout.output_rows;
        if output_rows == 0 {
            return false;
        }
        let text_cols = layout.text_cols.min(128);
        if text_cols == 0 {
            return false;
        }
        let max_scroll_before = before_lines.saturating_sub(output_rows);
        let pinned_before = scroll_before >= max_scroll_before;
        let delta = after_lines.saturating_sub(before_lines);
        let input_focus = self.input_focus == Some(idx);
        if !pinned_before {
            let _ = self.redraw_terminal_input_row(idx);
            if let Some(scrollbar) = self.terminal_scrollbar_draw(&layout, after_lines) {
                self.draw_scrollbar(win.id, scrollbar.track, scrollbar.thumb);
            }
            return true;
        }
        if before_lines < output_rows {
            return false;
        }
        if delta >= output_rows {
            return false;
        }
        let char_w = self.char_w.max(1);
        let char_h = self.char_h.max(1);
        if char_w == 0 || char_h == 0 {
            return false;
        }
        let x_px = layout.area.x.saturating_mul(char_w);
        let y_px = layout.area.y.saturating_mul(char_h);
        let w_px = text_cols.saturating_mul(char_w);
        let h_px = output_rows.saturating_mul(char_h);
        let dy_px = -((delta as i32) * (char_h as i32));
        console::layer_scroll_rect(win.id, x_px, y_px, w_px, h_px, dy_px, WINDOW_BG);
        let input_row_px = layout.area.y.saturating_add(output_rows).saturating_mul(char_h);
        let row_w_px = layout.area.w.saturating_mul(char_w);
        console::layer_fill_rect(win.id, x_px, input_row_px, row_w_px, char_h, WINDOW_BG);
        terminal::with_state(|term| {
            let draw_start = after_lines.saturating_sub(delta);
            let draw_row_start = layout.area.y.saturating_add(output_rows.saturating_sub(delta));
            self.draw_terminal_output_range(
                win.id,
                layout.area.x,
                draw_row_start,
                text_cols,
                delta,
                draw_start,
                term,
            );
            self.draw_terminal_input_row(win, &layout, input_focus, term);
        });
        if let Some(scrollbar) = self.terminal_scrollbar_draw(&layout, after_lines) {
            self.draw_scrollbar(win.id, scrollbar.track, scrollbar.thumb);
        }
        true
    }

    fn draw_text_cursor(&self, id: LayerId, col: usize, row: usize, color: u32) {
        let char_w = self.char_w.max(1);
        let char_h = self.char_h.max(1);
        if char_w == 0 || char_h == 0 {
            return;
        }
        let thickness = (char_h / 8).max(1).min(3);
        let x = col.saturating_mul(char_w);
        let y = row
            .saturating_mul(char_h)
            .saturating_add(char_h.saturating_sub(thickness));
        console::layer_fill_rect(id, x, y, char_w, thickness, color);
    }

    fn notes_layout(&self, win: &Window, total_lines: usize) -> Option<NotesLayout> {
        let area = self.content_area(win)?;
        let view_rows = area.h;
        if view_rows == 0 {
            return None;
        }
        let needs_scroll = total_lines > view_rows;
        let reserved = if needs_scroll { SCROLLBAR_COLS + SCROLLBAR_GAP_COLS } else { 0 };
        if area.w <= reserved {
            return None;
        }
        let text_cols = area.w.saturating_sub(reserved);
        let scrollbar_col = if needs_scroll {
            Some(area.x.saturating_add(text_cols + SCROLLBAR_GAP_COLS))
        } else {
            None
        };
        Some(NotesLayout { area, view_rows, text_cols, scrollbar_col })
    }

    fn terminal_layout_for_area(&self, area: ContentArea, needs_scroll: bool) -> Option<TerminalLayout> {
        if area.h == 0 {
            return None;
        }
        let output_rows = area.h.saturating_sub(1);
        let reserved = if needs_scroll { SCROLLBAR_COLS + SCROLLBAR_GAP_COLS } else { 0 };
        if area.w <= reserved {
            return None;
        }
        let text_cols = area.w.saturating_sub(reserved);
        let scrollbar_col = if needs_scroll {
            Some(area.x.saturating_add(text_cols + SCROLLBAR_GAP_COLS))
        } else {
            None
        };
        Some(TerminalLayout { area, output_rows, text_cols, scrollbar_col })
    }

    fn terminal_layout_and_lines(&self, win: &Window) -> Option<(TerminalLayout, usize)> {
        let area = self.content_area(win)?;
        let output_rows = area.h.saturating_sub(1);
        let mut layout = self.terminal_layout_for_area(area, false)?;
        let mut text_cols = layout.text_cols.min(128);
        let mut total_lines = terminal::visual_line_count(text_cols);
        let needs_scroll = output_rows > 0 && total_lines > output_rows;
        if needs_scroll {
            if let Some(scrolled) = self.terminal_layout_for_area(area, true) {
                layout = scrolled;
                text_cols = layout.text_cols.min(128);
                total_lines = terminal::visual_line_count(text_cols);
            }
        }
        terminal::set_view(layout.output_rows, text_cols);
        Some((layout, total_lines))
    }

    fn notes_scrollbar_draw(
        &self,
        win: &Window,
        layout: &NotesLayout,
        view_rows: usize,
        total_lines: usize,
    ) -> Option<ScrollbarDraw> {
        let Some(notes) = win.notes.as_ref() else { return None; };
        if total_lines <= view_rows {
            return None;
        }
        let char_w = self.char_w.max(1);
        let char_h = self.char_h.max(1);
        let scrollbar_col = layout.scrollbar_col?;
        if char_h == 0 {
            return None;
        }
        let track = Rect {
            x: scrollbar_col.saturating_mul(char_w),
            y: layout.area.y.saturating_mul(char_h),
            w: char_w,
            h: view_rows.saturating_mul(char_h),
        };
        let max_scroll = total_lines.saturating_sub(view_rows);
        let thumb_h = (track.h.saturating_mul(view_rows) / total_lines)
            .max(char_h)
            .min(track.h);
        let available = track.h.saturating_sub(thumb_h);
        let scroll = notes.scroll.min(max_scroll);
        let thumb_y = if max_scroll == 0 {
            track.y
        } else {
            track.y.saturating_add(available.saturating_mul(scroll) / max_scroll)
        };
        let thumb = Rect { x: track.x, y: thumb_y, w: track.w, h: thumb_h };
        Some(ScrollbarDraw { track, thumb })
    }

    fn terminal_scrollbar_draw(
        &self,
        layout: &TerminalLayout,
        total_lines: usize,
    ) -> Option<ScrollbarDraw> {
        if layout.output_rows == 0 || total_lines <= layout.output_rows {
            return None;
        }
        let scrollbar_col = layout.scrollbar_col?;
        let char_w = self.char_w.max(1);
        let char_h = self.char_h.max(1);
        let track = Rect {
            x: scrollbar_col.saturating_mul(char_w),
            y: layout.area.y.saturating_mul(char_h),
            w: char_w,
            h: layout.output_rows.saturating_mul(char_h),
        };
        let max_scroll = total_lines.saturating_sub(layout.output_rows);
        let thumb_h = (track.h.saturating_mul(layout.output_rows) / total_lines)
            .max(char_h)
            .min(track.h);
        let available = track.h.saturating_sub(thumb_h);
        let scroll = terminal::scroll().min(max_scroll);
        let thumb_y = if max_scroll == 0 {
            track.y
        } else {
            track.y.saturating_add(available.saturating_mul(scroll) / max_scroll)
        };
        let thumb = Rect { x: track.x, y: thumb_y, w: track.w, h: thumb_h };
        Some(ScrollbarDraw { track, thumb })
    }

    fn draw_scrollbar(&self, id: LayerId, track: Rect, thumb: Rect) {
        if track.w == 0 || track.h == 0 {
            return;
        }
        console::layer_fill_rect(id, track.x, track.y, track.w, track.h, SCROLLBAR_BG);
        if thumb.w == 0 || thumb.h == 0 {
            return;
        }
        console::layer_fill_rect(id, thumb.x, thumb.y, thumb.w, thumb.h, SCROLLBAR_THUMB);
    }

    fn draw_taskbar(&mut self) {
        let Some(id) = self.taskbar else { return; };
        console::layer_clear(id, TASKBAR_BG);
        console::layer_fill_rect(id, 0, 0, self.screen_w, 2, TASKBAR_ACCENT);
        self.taskbar_items.clear();
        self.start_button = None;

        let char_w = self.char_w.max(1);
        let char_h = self.char_h.max(1);
        let max_cols = self.screen_w / char_w;
        if max_cols == 0 || char_h == 0 {
            return;
        }

        let start_label = "Start";
        let start_cols = (start_label.chars().count() + 2).min(max_cols);
        let start_w_px = start_cols * char_w;
        let start_bg = if self.start_open { TASKBAR_ITEM_ACTIVE_BG } else { TASKBAR_ITEM_BG };
        console::layer_fill_rect(id, 0, 0, start_w_px, self.taskbar_h, start_bg);
        console::layer_draw_text_at_char(id, 1, 0, start_label, TASKBAR_FG, start_bg);
        self.start_button = Some(Rect {
            x: 0,
            y: self.taskbar_y,
            w: start_w_px,
            h: self.taskbar_h,
        });

        let time_str = time::format_hud_time();
        let mut right = HString::<64>::new();
        let _ = write!(&mut right, "{}", time_str.as_str());
        let right_len = right.chars().count();
        let draw_right = right_len + start_cols + 3 <= max_cols;
        let right_start_col = if draw_right {
            max_cols.saturating_sub(right_len + 1)
        } else {
            max_cols
        };
        if draw_right {
            console::layer_draw_text_at_char(id, right_start_col, 0, right.as_str(), TASKBAR_FG, TASKBAR_BG);
        }

        let end_col = right_start_col.saturating_sub(1);
        let mut col = start_cols.saturating_add(1);
        for (idx, win) in self.windows.iter().enumerate() {
            if col + 3 >= end_col {
                break;
            }
            let title = win.title.as_str();
            let avail = end_col.saturating_sub(col + 2);
            if avail == 0 {
                break;
            }
            let title_cols = title.chars().count().min(avail).min(24);
            if title_cols == 0 {
                break;
            }
            let item_cols = title_cols + 2;
            let item_x_px = col * char_w;
            let item_w_px = item_cols * char_w;
            let bg = if self.focused == Some(idx) {
                TASKBAR_ITEM_ACTIVE_BG
            } else {
                TASKBAR_ITEM_BG
            };
            console::layer_fill_rect(id, item_x_px, 0, item_w_px, self.taskbar_h, bg);
            let mut label = HString::<32>::new();
            for ch in title.chars().take(title_cols) {
                let _ = label.push(ch);
            }
            console::layer_draw_text_at_char(id, col + 1, 0, label.as_str(), TASKBAR_FG, bg);
            self.taskbar_items.push(TaskbarItem {
                window: idx,
                rect: Rect {
                    x: item_x_px,
                    y: self.taskbar_y,
                    w: item_w_px,
                    h: self.taskbar_h,
                },
            });
            col = col.saturating_add(item_cols + 1);
        }
    }

    fn is_in_taskbar(&self, y: usize) -> bool {
        y >= self.taskbar_y
    }

    fn taskbar_item_at(&self, x: usize, y: usize) -> Option<usize> {
        for item in &self.taskbar_items {
            if item.rect.contains(x, y) {
                if item.window < self.windows.len() {
                    return Some(item.window);
                }
            }
        }
        None
    }

    fn scroll_target_at(&self, x: usize, y: usize) -> Option<ScrollTarget> {
        if self.start_open && self.start_menu_contains(x, y) {
            return Some(ScrollTarget::StartMenu);
        }
        let idx = self.window_at(x, y)?;
        let win = self.windows.get(idx)?;
        if win.minimized {
            return None;
        }
        let rect = self.content_rect_px(win)?;
        if !rect.contains(x, y) {
            return None;
        }
        match win.kind {
            WindowKind::Notes => Some(ScrollTarget::Notes(idx)),
            WindowKind::Terminal => Some(ScrollTarget::Terminal(idx)),
            _ => None,
        }
    }

    fn scroll_target_for_window(&self, idx: usize) -> Option<ScrollTarget> {
        let win = self.windows.get(idx)?;
        if win.minimized {
            return None;
        }
        match win.kind {
            WindowKind::Notes => Some(ScrollTarget::Notes(idx)),
            WindowKind::Terminal => Some(ScrollTarget::Terminal(idx)),
            _ => None,
        }
    }

    fn scrollbar_hit_at(&self, x: usize, y: usize) -> Option<ScrollbarInfo> {
        if self.start_open {
            if let Some(info) = self.scrollbar_info_for_target(ScrollTarget::StartMenu) {
                if info.track.contains(x, y) {
                    return Some(info);
                }
            }
        }
        if let Some(idx) = self.window_at(x, y) {
            if let Some(win) = self.windows.get(idx) {
                let target = match win.kind {
                    WindowKind::Notes => Some(ScrollTarget::Notes(idx)),
                    WindowKind::Terminal => Some(ScrollTarget::Terminal(idx)),
                    _ => None,
                };
                if let Some(target) = target {
                    if let Some(info) = self.scrollbar_info_for_target(target) {
                        if info.track.contains(x, y) {
                            return Some(info);
                        }
                    }
                }
            }
        }
        None
    }

    fn scroll_to_thumb(&mut self, target: ScrollTarget, pointer_y: i32, grab_offset: i32) -> bool {
        let Some(metrics) = self.scroll_metrics(target) else { return false; };
        let track_y = metrics.track.y as i32;
        let available = metrics.track.h.saturating_sub(metrics.thumb_h) as i32;
        let mut thumb_y = pointer_y - grab_offset - track_y;
        if thumb_y < 0 {
            thumb_y = 0;
        }
        if thumb_y > available {
            thumb_y = available;
        }
        let max_scroll = metrics.max_scroll;
        let new_scroll = if max_scroll == 0 || available <= 0 {
            0
        } else {
            ((thumb_y as i64) * (max_scroll as i64) / (available as i64)) as usize
        };
        match target {
            ScrollTarget::Notes(idx) => self.set_notes_scroll(idx, new_scroll),
            ScrollTarget::Terminal(idx) => self.set_terminal_scroll(idx, new_scroll),
            ScrollTarget::StartMenu => self.set_start_menu_scroll(new_scroll),
        }
    }

    fn scroll_metrics(&self, target: ScrollTarget) -> Option<ScrollMetrics> {
        match target {
            ScrollTarget::Notes(idx) => self.notes_scroll_metrics(idx),
            ScrollTarget::Terminal(idx) => self.terminal_scroll_metrics(idx),
            ScrollTarget::StartMenu => self.start_menu_scroll_metrics(),
        }
    }

    fn scrollbar_info_for_target(&self, target: ScrollTarget) -> Option<ScrollbarInfo> {
        let metrics = self.scroll_metrics(target)?;
        let scroll = match target {
            ScrollTarget::Notes(idx) => {
                let win = self.windows.get(idx)?;
                win.notes.as_ref()?.scroll
            }
            ScrollTarget::Terminal(_) => terminal::scroll(),
            ScrollTarget::StartMenu => self.start_menu_scroll,
        };
        let scroll = scroll.min(metrics.max_scroll);
        let available = metrics.track.h.saturating_sub(metrics.thumb_h);
        let thumb_y = if metrics.max_scroll == 0 {
            metrics.track.y
        } else {
            metrics.track.y.saturating_add(available.saturating_mul(scroll) / metrics.max_scroll)
        };
        let thumb = Rect {
            x: metrics.track.x,
            y: thumb_y,
            w: metrics.track.w,
            h: metrics.thumb_h,
        };
        Some(ScrollbarInfo {
            target,
            track: metrics.track,
            thumb,
        })
    }

    fn notes_scroll_metrics(&self, idx: usize) -> Option<ScrollMetrics> {
        let win = self.windows.get(idx)?;
        if win.minimized {
            return None;
        }
        let notes = win.notes.as_ref()?;
        let layout = self.notes_layout(win, notes.lines.len())?;
        let view_rows = layout.view_rows;
        if notes.lines.len() <= view_rows {
            return None;
        }
        let scrollbar_col = layout.scrollbar_col?;
        let char_w = self.char_w.max(1);
        let char_h = self.char_h.max(1);
        let track = Rect {
            x: win.x.saturating_add(scrollbar_col.saturating_mul(char_w)),
            y: win.y.saturating_add(layout.area.y.saturating_mul(char_h)),
            w: char_w,
            h: view_rows.saturating_mul(char_h),
        };
        let max_scroll = notes.lines.len().saturating_sub(view_rows);
        let thumb_h = (track.h.saturating_mul(view_rows) / notes.lines.len())
            .max(char_h)
            .min(track.h);
        Some(ScrollMetrics { track, thumb_h, max_scroll })
    }

    fn start_menu_scroll_metrics(&self) -> Option<ScrollMetrics> {
        if !self.start_open {
            return None;
        }
        let rect = self.start_menu_rect?;
        let total = START_MENU_ITEMS.len();
        let visible = self.start_menu_visible_rows();
        if visible == 0 || total <= visible {
            return None;
        }
        let char_w = self.char_w.max(1);
        let char_h = self.char_h.max(1);
        if char_w == 0 || char_h == 0 {
            return None;
        }
        let track = Rect {
            x: rect.x.saturating_add(rect.w.saturating_sub(char_w)),
            y: rect.y,
            w: char_w,
            h: visible.saturating_mul(char_h),
        };
        let max_scroll = total.saturating_sub(visible);
        let thumb_h = (track.h.saturating_mul(visible) / total)
            .max(char_h)
            .min(track.h);
        Some(ScrollMetrics { track, thumb_h, max_scroll })
    }

    fn terminal_scroll_metrics(&self, idx: usize) -> Option<ScrollMetrics> {
        let win = self.windows.get(idx)?;
        if win.minimized {
            return None;
        }
        let (layout, total_lines) = self.terminal_layout_and_lines(win)?;
        if layout.output_rows == 0 || total_lines <= layout.output_rows {
            return None;
        }
        let scrollbar_col = layout.scrollbar_col?;
        let char_w = self.char_w.max(1);
        let char_h = self.char_h.max(1);
        let track = Rect {
            x: win.x.saturating_add(scrollbar_col.saturating_mul(char_w)),
            y: win.y.saturating_add(layout.area.y.saturating_mul(char_h)),
            w: char_w,
            h: layout.output_rows.saturating_mul(char_h),
        };
        let max_scroll = total_lines.saturating_sub(layout.output_rows);
        let thumb_h = (track.h.saturating_mul(layout.output_rows) / total_lines)
            .max(char_h)
            .min(track.h);
        Some(ScrollMetrics { track, thumb_h, max_scroll })
    }

    fn handle_taskbar_click(&mut self, x: usize, y: usize) -> bool {
        if let Some(rect) = self.start_button {
            if rect.contains(x, y) {
                self.set_start_menu_open(!self.start_open);
                return true;
            }
        }

        let Some(idx) = self.taskbar_item_at(x, y) else {
            return false;
        };
        self.set_start_menu_open(false);
        let was_focused = self.focused == Some(idx);
        let minimized = self.windows.get(idx).map(|win| win.minimized).unwrap_or(false);

        if was_focused && !minimized {
            if self.set_window_minimized(idx, true) {
                if let Some(next) = self.next_visible(idx) {
                    self.focus_window(next, FocusInput::Auto);
                } else {
                    self.focused = None;
                    self.input_focus = None;
                }
            }
        } else {
            self.focus_window(idx, FocusInput::Auto);
        }
        true
    }

    fn scroll_by_lines(&mut self, target: ScrollTarget, lines: i32) -> bool {
        if lines == 0 {
            return false;
        }
        match target {
            ScrollTarget::Notes(idx) => self.scroll_notes(idx, lines),
            ScrollTarget::Terminal(idx) => self.scroll_terminal(idx, lines),
            ScrollTarget::StartMenu => self.scroll_start_menu(lines),
        }
    }

    fn set_notes_scroll(&mut self, idx: usize, scroll: usize) -> bool {
        let max_scroll = {
            let Some(win) = self.windows.get(idx) else { return false; };
            if win.minimized {
                return false;
            }
            let Some(notes) = win.notes.as_ref() else { return false; };
            let view_rows = self.notes_layout(win, notes.lines.len()).map(|layout| layout.view_rows).unwrap_or(0);
            notes.max_scroll(view_rows)
        };
        let Some(win) = self.windows.get_mut(idx) else { return false; };
        if win.minimized {
            return false;
        }
        let Some(notes) = win.notes.as_mut() else { return false; };
        let scroll = scroll.min(max_scroll);
        if scroll == notes.scroll {
            return false;
        }
        notes.scroll = scroll;
        let active = self.focused == Some(idx);
        self.draw_window(idx, active);
        true
    }

    fn scroll_notes(&mut self, idx: usize, lines: i32) -> bool {
        let active = self.focused == Some(idx);
        let view_rows = {
            let Some(win) = self.windows.get(idx) else { return false; };
            if win.minimized {
                return false;
            }
            let Some(notes) = win.notes.as_ref() else { return false; };
            self.notes_layout(win, notes.lines.len()).map(|layout| layout.view_rows).unwrap_or(0)
        };
        let changed = {
            let Some(win) = self.windows.get_mut(idx) else { return false; };
            if win.minimized {
                return false;
            }
            let Some(notes) = win.notes.as_mut() else { return false; };
            notes.scroll_by(lines, view_rows)
        };
        if !changed {
            return false;
        }
        self.draw_window(idx, active);
        true
    }

    fn set_terminal_scroll(&mut self, idx: usize, scroll: usize) -> bool {
        let Some(win) = self.windows.get(idx) else { return false; };
        if win.minimized {
            return false;
        }
        let Some((layout, _)) = self.terminal_layout_and_lines(win) else { return false; };
        if layout.output_rows == 0 {
            return false;
        }
        if !terminal::set_scroll(scroll) {
            return false;
        }
        let active = self.focused == Some(idx);
        self.draw_window(idx, active);
        true
    }

    fn scroll_terminal(&mut self, idx: usize, lines: i32) -> bool {
        let Some(win) = self.windows.get(idx) else { return false; };
        if win.minimized {
            return false;
        }
        let Some((layout, _)) = self.terminal_layout_and_lines(win) else { return false; };
        if layout.output_rows == 0 {
            return false;
        }
        let changed = terminal::scroll_by(lines);
        if !changed {
            return false;
        }
        let active = self.focused == Some(idx);
        self.draw_window(idx, active);
        true
    }

    fn start_menu_visible_rows(&self) -> usize {
        if START_MENU_ITEMS.is_empty() {
            return 0;
        }
        let char_h = self.char_h.max(1);
        if char_h == 0 {
            return 0;
        }
        let max_rows = self.taskbar_y / char_h;
        let max_rows = max_rows.max(1);
        START_MENU_ITEMS.len().min(max_rows)
    }

    fn scroll_start_menu(&mut self, lines: i32) -> bool {
        if !self.start_open {
            return false;
        }
        let visible = self.start_menu_visible_rows();
        if visible == 0 {
            return false;
        }
        let max_scroll = START_MENU_ITEMS.len().saturating_sub(visible) as i32;
        let mut new_scroll = self.start_menu_scroll as i32 + lines;
        if new_scroll < 0 {
            new_scroll = 0;
        }
        if new_scroll > max_scroll {
            new_scroll = max_scroll;
        }
        self.set_start_menu_scroll(new_scroll as usize)
    }

    fn set_start_menu_scroll(&mut self, scroll: usize) -> bool {
        if !self.start_open {
            return false;
        }
        let visible = self.start_menu_visible_rows();
        if visible == 0 {
            return false;
        }
        let max_scroll = START_MENU_ITEMS.len().saturating_sub(visible);
        let scroll = scroll.min(max_scroll);
        if scroll == self.start_menu_scroll {
            return false;
        }
        self.start_menu_scroll = scroll;
        self.draw_start_menu();
        true
    }

    fn set_start_menu_open(&mut self, open: bool) -> bool {
        if self.start_open == open {
            return false;
        }
        self.start_open = open;
        if open {
            self.start_menu_scroll = 0;
            self.draw_start_menu();
        } else {
            if matches!(self.scroll_drag.as_ref().map(|drag| drag.target), Some(ScrollTarget::StartMenu)) {
                self.scroll_drag = None;
            }
            self.hide_start_menu();
        }
        true
    }

    fn start_menu_contains(&self, x: usize, y: usize) -> bool {
        if !self.start_open {
            return false;
        }
        self.start_menu_rect.map(|rect| rect.contains(x, y)).unwrap_or(false)
    }

    fn start_menu_action_at(&self, x: usize, y: usize) -> Option<StartAction> {
        if !self.start_open {
            return None;
        }
        let rect = self.start_menu_rect?;
        if !rect.contains(x, y) {
            return None;
        }
        let needs_scroll = START_MENU_ITEMS.len() > self.start_menu_visible_rows();
        if needs_scroll {
            let char_w = self.char_w.max(1);
            if char_w > 0 && x >= rect.x.saturating_add(rect.w.saturating_sub(char_w)) {
                return None;
            }
        }
        let char_h = self.char_h.max(1);
        if char_h == 0 {
            return None;
        }
        let row = (y - rect.y) / char_h;
        let idx = self.start_menu_scroll.saturating_add(row);
        START_MENU_ITEMS.get(idx).map(|(_, action)| *action)
    }

    fn handle_start_action(&mut self, action: StartAction) {
        match action {
            StartAction::Notes => self.spawn_notes_window(),
            StartAction::Help => self.spawn_help_window(),
            StartAction::Terminal => self.spawn_terminal_window(),
        }
    }

    fn draw_start_menu(&mut self) {
        if START_MENU_ITEMS.is_empty() {
            return;
        }
        let char_w = self.char_w.max(1);
        let char_h = self.char_h.max(1);
        if char_w == 0 || char_h == 0 {
            return;
        }
        let max_len = START_MENU_ITEMS
            .iter()
            .map(|(label, _)| label.chars().count())
            .max()
            .unwrap_or(0);
        let visible_rows = self.start_menu_visible_rows();
        if visible_rows == 0 {
            return;
        }
        let max_scroll = START_MENU_ITEMS.len().saturating_sub(visible_rows);
        if self.start_menu_scroll > max_scroll {
            self.start_menu_scroll = max_scroll;
        }
        let needs_scroll = START_MENU_ITEMS.len() > visible_rows;
        let menu_cols = max_len.saturating_add(2 + if needs_scroll { 1 } else { 0 });
        let menu_rows = visible_rows;
        let mut menu_w = menu_cols.saturating_mul(char_w);
        let mut menu_h = menu_rows.saturating_mul(char_h);
        if menu_w == 0 || menu_h == 0 {
            return;
        }
        menu_w = menu_w.min(self.screen_w.max(1));
        menu_h = menu_h.min(self.taskbar_y.max(1));

        let start_x = self.start_button.map(|rect| rect.x).unwrap_or(0);
        let x = start_x.min(self.screen_w.saturating_sub(menu_w));
        let y = self.taskbar_y.saturating_sub(menu_h);
        let rect = Rect { x, y, w: menu_w, h: menu_h };

        let needs_new = match self.start_menu_rect {
            Some(old) => old.x != rect.x || old.y != rect.y || old.w != rect.w || old.h != rect.h,
            None => true,
        };
        if needs_new {
            if let Some(id) = self.start_menu.take() {
                console::destroy_layer(id);
            }
            let id = console::create_layer(rect.w, rect.h, rect.x, rect.y, START_MENU_Z, 255);
            self.start_menu = id;
            self.start_menu_rect = id.map(|_| rect);
        } else if let Some(id) = self.start_menu {
            console::layer_set_pos(id, rect.x, rect.y);
        }

        let Some(id) = self.start_menu else { return; };
        console::layer_set_visible(id, true);
        console::layer_clear(id, START_MENU_BG);

        let border = BORDER_THICKNESS.min(rect.w).min(rect.h);
        if border > 0 {
            console::layer_fill_rect(id, 0, 0, rect.w, border, START_MENU_BORDER);
            if rect.h > border {
                console::layer_fill_rect(id, 0, rect.h - border, rect.w, border, START_MENU_BORDER);
            }
            console::layer_fill_rect(id, 0, 0, border, rect.h, START_MENU_BORDER);
            if rect.w > border {
                console::layer_fill_rect(id, rect.w - border, 0, border, rect.h, START_MENU_BORDER);
            }
        }

        for (i, (label, _)) in START_MENU_ITEMS
            .iter()
            .skip(self.start_menu_scroll)
            .take(visible_rows)
            .enumerate()
        {
            console::layer_draw_text_at_char(id, 1, i, label, TASKBAR_FG, START_MENU_BG);
        }

        if let Some(info) = self.scrollbar_info_for_target(ScrollTarget::StartMenu) {
            let track = Rect {
                x: info.track.x.saturating_sub(rect.x),
                y: info.track.y.saturating_sub(rect.y),
                w: info.track.w,
                h: info.track.h,
            };
            let thumb = Rect {
                x: info.thumb.x.saturating_sub(rect.x),
                y: info.thumb.y.saturating_sub(rect.y),
                w: info.thumb.w,
                h: info.thumb.h,
            };
            self.draw_scrollbar(id, track, thumb);
        }
    }

    fn hide_start_menu(&mut self) {
        if let Some(id) = self.start_menu {
            console::layer_set_visible(id, false);
        }
    }

    fn update_cursor(&mut self) -> bool {
        if self.cursor.is_none() {
            self.ensure_cursor_layer();
        }
        let Some(id) = self.cursor else { return false; };
        let (mx, my) = mouse::position();
        let buttons = mouse::buttons();
        let max_x = self.screen_w.saturating_sub(self.cursor_size);
        let max_y = self.screen_h.saturating_sub(self.cursor_size);
        let hot_x = CURSOR_HOT_X.min(self.cursor_size.saturating_sub(1));
        let hot_y = CURSOR_HOT_Y.min(self.cursor_size.saturating_sub(1));
        let mut x = mx.saturating_sub(hot_x);
        let mut y = my.saturating_sub(hot_y);
        if x > max_x { x = max_x; }
        if y > max_y { y = max_y; }
        let mut changed = false;
        if buttons != self.cursor_buttons {
            self.cursor_buttons = buttons;
            let pressed = (buttons & 0x07) != 0;
            draw_cursor_layer(id, self.cursor_size, pressed);
            changed = true;
        }
        if x == self.cursor_x && y == self.cursor_y {
            return changed;
        }
        self.cursor_x = x;
        self.cursor_y = y;
        console::layer_set_pos(id, x, y);
        true
    }

    fn ensure_cursor_layer(&mut self) {
        if self.cursor.is_some() {
            return;
        }
        let id = console::create_layer(self.cursor_size, self.cursor_size, 0, 0, CURSOR_Z, 255);
        if let Some(id) = id {
            draw_cursor_layer(id, self.cursor_size, false);
            self.cursor = Some(id);
            self.cursor_x = usize::MAX;
            self.cursor_y = usize::MAX;
        }
    }

    fn close_window_idx(&mut self, idx: usize) {
        if idx >= self.windows.len() {
            return;
        }
        let was_focused = self.focused == Some(idx);
        let win = self.windows.remove(idx);
        console::destroy_layer(win.id);
        if let Some(drag) = &mut self.drag {
            if drag.window == idx {
                self.drag = None;
            } else if drag.window > idx {
                drag.window -= 1;
            }
        }
        if let Some(focus) = self.input_focus {
            if focus == idx {
                self.input_focus = None;
            } else if focus > idx {
                self.input_focus = Some(focus - 1);
            }
        }
        if let Some(focus) = self.focused {
            if focus == idx {
                self.focused = None;
            } else if focus > idx {
                self.focused = Some(focus - 1);
            }
        }
        if let Some(drag) = &mut self.scroll_drag {
            match drag.target {
                ScrollTarget::Notes(win_idx) => {
                    if win_idx == idx {
                        self.scroll_drag = None;
                    } else if win_idx > idx {
                        drag.target = ScrollTarget::Notes(win_idx - 1);
                    }
                }
                ScrollTarget::Terminal(win_idx) => {
                    if win_idx == idx {
                        self.scroll_drag = None;
                    } else if win_idx > idx {
                        drag.target = ScrollTarget::Terminal(win_idx - 1);
                    }
                }
                ScrollTarget::StartMenu => {}
            }
        }

        if self.windows.is_empty() {
            self.focused = None;
            self.input_focus = None;
            self.draw_taskbar();
            return;
        }

        if was_focused {
            let start = idx.min(self.windows.len() - 1);
            if let Some(next) = self.first_visible_from(start) {
                self.focus_window(next, FocusInput::Auto);
            } else {
                self.focused = None;
                self.input_focus = None;
            }
        }
        self.draw_taskbar();
    }

    fn set_window_minimized(&mut self, idx: usize, minimized: bool) -> bool {
        let id = {
            let Some(win) = self.windows.get_mut(idx) else { return false; };
            if win.minimized == minimized {
                return false;
            }
            win.minimized = minimized;
            win.id
        };
        let was_focused = self.focused == Some(idx);
        let was_input = self.input_focus == Some(idx);
        let was_drag = self.drag.as_ref().map(|drag| drag.window == idx).unwrap_or(false);

        console::layer_set_visible(id, !minimized);
        if minimized {
            if was_focused {
                self.focused = None;
            }
            if was_input {
                self.input_focus = None;
            }
            if was_drag {
                self.drag = None;
            }
            if matches!(self.scroll_drag.as_ref().map(|drag| drag.target), Some(ScrollTarget::Notes(win_idx) | ScrollTarget::Terminal(win_idx)) if win_idx == idx) {
                self.scroll_drag = None;
            }
        } else {
            let active = self.focused == Some(idx);
            self.draw_window(idx, active);
        }
        true
    }

    fn resize_window_to(&mut self, idx: usize, new_w: usize, new_h: usize) {
        let win = self.windows.get(idx).cloned();
        let Some(win) = win else {
            self.drag = None;
            return;
        };
        if new_w == win.w && new_h == win.h {
            return;
        }
        self.rebuild_window(idx, win, new_w, new_h);
        if idx >= self.windows.len() {
            self.drag = None;
        }
    }

    fn rebuild_window(&mut self, idx: usize, old: Window, new_w: usize, new_h: usize) {
        console::destroy_layer(old.id);
        let new_id = console::create_layer(new_w, new_h, old.x, old.y, old.z, 255);
        if let Some(new_id) = new_id {
            let mut win = old;
            win.id = new_id;
            win.w = new_w;
            win.h = new_h;
            let lines_len = win.notes.as_ref().map(|notes| notes.lines.len()).unwrap_or(0);
            let view_rows = self.notes_layout(&win, lines_len).map(|layout| layout.view_rows).unwrap_or(0);
            if let Some(notes) = win.notes.as_mut() {
                notes.clamp_scroll(view_rows);
            }
            self.windows[idx] = win;
            let active = self.focused == Some(idx);
            self.draw_window(idx, active);
            return;
        }

        let fallback_id = console::create_layer(old.w, old.h, old.x, old.y, old.z, 255);
        if let Some(fallback_id) = fallback_id {
            let mut win = old;
            win.id = fallback_id;
            let lines_len = win.notes.as_ref().map(|notes| notes.lines.len()).unwrap_or(0);
            let view_rows = self.notes_layout(&win, lines_len).map(|layout| layout.view_rows).unwrap_or(0);
            if let Some(notes) = win.notes.as_mut() {
                notes.clamp_scroll(view_rows);
            }
            self.windows[idx] = win;
            let active = self.focused == Some(idx);
            self.draw_window(idx, active);
            return;
        }

        self.windows.remove(idx);
        if self.windows.is_empty() {
            self.focused = None;
            self.input_focus = None;
        } else if let Some(focused) = self.focused {
            let new_idx = focused.min(self.windows.len() - 1);
            self.focus_window(new_idx, FocusInput::Auto);
        }
    }

    fn min_window_w(&self) -> usize {
        self.char_w.saturating_mul(MIN_WINDOW_COLS).max(self.char_w * 6)
    }

    fn min_window_h(&self) -> usize {
        self.char_h.saturating_mul(MIN_WINDOW_ROWS).max(self.char_h * 4)
    }

    fn title_bar_height(&self) -> usize {
        self.char_h.saturating_add(6).max(self.char_h)
    }
}

fn draw_cursor_layer(id: LayerId, size: usize, pressed: bool) {
    let (fill, border_color) = if pressed {
        (CURSOR_FILL_ACTIVE, CURSOR_BORDER_ACTIVE)
    } else {
        (CURSOR_FILL, CURSOR_BORDER)
    };
    console::layer_clear(id, fill);
    let border = (size / 6).max(1).min(3);
    if border == 0 {
        return;
    }
    console::layer_fill_rect(id, 0, 0, size, border, border_color);
    if size > border {
        console::layer_fill_rect(id, 0, size - border, size, border, border_color);
    }
    console::layer_fill_rect(id, 0, 0, border, size, border_color);
    if size > border {
        console::layer_fill_rect(id, size - border, 0, border, size, border_color);
    }
    let dot = (size / 4).max(2).min(4);
    if dot < size {
        let start = (size - dot) / 2;
        console::layer_fill_rect(id, start, start, dot, dot, border_color);
    }
}

fn clamp_dim(value: i32, min: usize, max: usize) -> usize {
    let mut v = value;
    let min = min.max(1) as i32;
    let max = max.max(1) as i32;
    if v < min {
        v = min;
    }
    if v > max {
        v = max;
    }
    v as usize
}

fn parse_rgb_hex(s: &str) -> Option<u32> {
    let h = s.trim();
    if h.len() == 3 {
        let mut buf = [0u8; 6];
        for (i, b) in h.bytes().enumerate() {
            buf[i * 2] = b;
            buf[i * 2 + 1] = b;
        }
        let expanded = core::str::from_utf8(&buf).ok()?;
        return u32::from_str_radix(expanded, 16).ok();
    }
    if h.len() == 6 {
        return u32::from_str_radix(h, 16).ok();
    }
    None
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

fn delete_selection(line: &mut HString<128>, cursor_pos: &mut usize, anchor: usize) -> bool {
    let start = anchor.min(*cursor_pos);
    let end = anchor.max(*cursor_pos);
    if start == end {
        return false;
    }
    let mut new_line = HString::<128>::new();
    for (i, ch) in line.chars().enumerate() {
        if i >= start && i < end {
            continue;
        }
        let _ = new_line.push(ch);
    }
    *line = new_line;
    *cursor_pos = start;
    true
}

fn insert_char_at(line: &mut HString<128>, idx: usize, ch: char) -> bool {
    let len = line.chars().count();
    if idx > len {
        return false;
    }
    let mut new_line = HString::<128>::new();
    let mut inserted = false;
    for (i, existing) in line.chars().enumerate() {
        if i == idx {
            if new_line.push(ch).is_err() {
                return false;
            }
            inserted = true;
        }
        if new_line.push(existing).is_err() {
            return false;
        }
    }
    if !inserted && new_line.push(ch).is_err() {
        return false;
    }
    *line = new_line;
    true
}

fn remove_char_at(line: &mut HString<128>, idx: usize) -> bool {
    let len = line.chars().count();
    if idx >= len {
        return false;
    }
    let mut new_line = HString::<128>::new();
    for (i, ch) in line.chars().enumerate() {
        if i == idx {
            continue;
        }
        if new_line.push(ch).is_err() {
            return false;
        }
    }
    *line = new_line;
    true
}

fn delete_prev_word(line: &mut HString<128>, cursor_pos: &mut usize) -> bool {
    if *cursor_pos == 0 {
        return false;
    }
    let mut chars: Vec<char> = line.chars().collect();
    let mut idx = (*cursor_pos).min(chars.len());
    while idx > 0 && chars[idx - 1].is_ascii_whitespace() {
        idx -= 1;
    }
    while idx > 0 && !chars[idx - 1].is_ascii_whitespace() {
        idx -= 1;
    }
    if idx == *cursor_pos {
        return false;
    }
    let remove_count = *cursor_pos - idx;
    for _ in 0..remove_count {
        chars.remove(idx);
    }
    line.clear();
    for ch in chars.iter() {
        let _ = line.push(*ch);
    }
    *cursor_pos = idx;
    true
}

fn move_cursor_word_left(line: &HString<128>, cursor_pos: &mut usize) -> bool {
    if *cursor_pos == 0 {
        return false;
    }
    let chars: Vec<char> = line.chars().collect();
    let mut idx = (*cursor_pos).min(chars.len());
    while idx > 0 && chars[idx - 1].is_ascii_whitespace() {
        idx -= 1;
    }
    while idx > 0 && !chars[idx - 1].is_ascii_whitespace() {
        idx -= 1;
    }
    if idx == *cursor_pos {
        return false;
    }
    *cursor_pos = idx;
    true
}

fn move_cursor_word_right(line: &HString<128>, cursor_pos: &mut usize) -> bool {
    let chars: Vec<char> = line.chars().collect();
    if *cursor_pos >= chars.len() {
        return false;
    }
    let mut idx = *cursor_pos;
    while idx < chars.len() && !chars[idx].is_ascii_whitespace() {
        idx += 1;
    }
    while idx < chars.len() && chars[idx].is_ascii_whitespace() {
        idx += 1;
    }
    if idx == *cursor_pos {
        return false;
    }
    *cursor_pos = idx;
    true
}
