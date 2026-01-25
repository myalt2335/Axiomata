use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::cmp::Ordering;
use core::fmt::Write;
use heapless::String as HString;

use crate::clipboard;
use crate::commands;
use crate::console;
use crate::debug;
use crate::fs;
use crate::history;
use crate::keyboard::KeyEvent;
use crate::terminal;
use crate::windows::{
    apply_intensity, AppAction, AppContext, AppDescriptor, AppEventResult, ContentArea, MouseEvent, MouseEventKind,
    Rect, ScrollMetrics, ScrollbarDraw, WindowApp,
};

const NOTES_MAX_LINES: usize = 64;
const NOTES_MAX_COLS: usize = 128;
const SCROLLBAR_COLS: usize = 1;
const SCROLLBAR_GAP_COLS: usize = 1;
const TERMINAL_PROMPT: &str = "> ";
const FILE_EXPLORER_HEADER_ROWS: usize = 1;
const FILE_EXPLORER_SIZE_COLS: usize = 8;
const FILE_EXPLORER_SIZE_GAP: usize = 1;
const APP_PICKER_MIN_COLS: usize = 18;
const APP_PICKER_MAX_COLS: usize = 32;
const APP_PICKER_MIN_ROWS: usize = 4;
const APP_PICKER_MAX_ROWS: usize = 8;

const WELCOME_TEXT: &[&str] = &[
    "Drag the title bar to move a window. Drag the corner to resize it.",
    "Alt+Tab switches windows. The Start key opens the menu.",
    "As the scroll wheel hasn't been perfected yet, simply use the scrollbar to navigate apps where applicable.",
];

pub fn builtin_apps() -> &'static [AppDescriptor] {
    BUILTIN_APPS
}

const BUILTIN_APPS: &[AppDescriptor] = &[
    AppDescriptor {
        label: "Welcome",
        default_title: "Welcome",
        start_menu: false,
        startup: true,
        openable: false,
        factory: create_welcome_app,
    },
    AppDescriptor {
        label: "Files",
        default_title: "Files",
        start_menu: true,
        startup: false,
        openable: false,
        factory: create_file_explorer_app,
    },
    AppDescriptor {
        label: "Notes",
        default_title: "Notes",
        start_menu: true,
        startup: false,
        openable: true,
        factory: create_notes_app,
    },
    AppDescriptor {
        label: "Help",
        default_title: "Help",
        start_menu: true,
        startup: false,
        openable: false,
        factory: create_help_app,
    },
    AppDescriptor {
        label: "Terminal",
        default_title: "Terminal",
        start_menu: true,
        startup: false,
        openable: false,
        factory: create_terminal_app,
    },
];

fn create_welcome_app() -> Box<dyn WindowApp> {
    Box::new(InfoApp { paragraphs: WELCOME_TEXT })
}

fn create_help_app() -> Box<dyn WindowApp> {
    Box::new(InfoApp { paragraphs: WELCOME_TEXT })
}

fn create_notes_app() -> Box<dyn WindowApp> {
    Box::new(NotesApp::new())
}

fn create_file_explorer_app() -> Box<dyn WindowApp> {
    Box::new(FileExplorerApp::new())
}

fn create_terminal_app() -> Box<dyn WindowApp> {
    Box::new(TerminalApp::new())
}

struct InfoApp {
    paragraphs: &'static [&'static str],
}

impl WindowApp for InfoApp {
    fn draw(&mut self, ctx: &mut AppContext, _input_focus: bool) {
        let Some(area) = ctx.metrics.content_area else { return; };
        ctx.draw_wrapped_paragraphs(area, self.paragraphs, ctx.colors.fg, ctx.colors.bg);
    }
}

#[derive(Clone)]
struct NotesBuffer {
    lines: Vec<HString<128>>,
    cursor_row: usize,
    cursor_col: usize,
    scroll: usize,
    selection_all: bool,
}

impl NotesBuffer {
    fn new() -> Self {
        let mut lines = Vec::new();
        lines.push(HString::<128>::new());
        Self { lines, cursor_row: 0, cursor_col: 0, scroll: 0, selection_all: false }
    }

    fn current_line_mut(&mut self) -> &mut HString<128> {
        if self.lines.is_empty() {
            self.lines.push(HString::<128>::new());
            self.cursor_row = 0;
            self.cursor_col = 0;
        }
        if self.cursor_row >= self.lines.len() {
            self.cursor_row = self.lines.len().saturating_sub(1);
        }
        let line = &mut self.lines[self.cursor_row];
        let len = Self::line_len(line);
        if self.cursor_col > len {
            self.cursor_col = len;
        }
        line
    }

    fn line_len(line: &HString<128>) -> usize {
        line.chars().count()
    }

    fn set_cursor(&mut self, row: usize, col: usize) -> bool {
        if self.lines.is_empty() {
            self.lines.push(HString::<128>::new());
        }
        let row = row.min(self.lines.len().saturating_sub(1));
        let max_col = Self::line_len(&self.lines[row]);
        let col = col.min(max_col);
        let changed = row != self.cursor_row || col != self.cursor_col || self.selection_all;
        self.cursor_row = row;
        self.cursor_col = col;
        self.selection_all = false;
        changed
    }

    fn push_char(&mut self, ch: char) -> bool {
        if self.selection_all {
            let _ = self.clear_all();
        }
        self.selection_all = false;
        let col = self.cursor_col;
        let (inserted, next_col) = {
            let line = self.current_line_mut();
            let len = Self::line_len(line);
            if len >= NOTES_MAX_COLS {
                return false;
            }
            let col = col.min(len);
            let next_col = col.saturating_add(1);
            (insert_char_at(line, col, ch), next_col)
        };
        if inserted {
            self.cursor_col = next_col;
            true
        } else {
            false
        }
    }

    fn backspace(&mut self) -> bool {
        if self.selection_all {
            return self.clear_all();
        }
        self.selection_all = false;
        if self.cursor_col > 0 {
            let mut col = self.cursor_col;
            let mut new_col = col.saturating_sub(1);
            let removed = {
                let line = self.current_line_mut();
                let len = Self::line_len(line);
                col = col.min(len);
                if col == 0 {
                    false
                } else {
                    new_col = col.saturating_sub(1);
                    remove_char_at(line, new_col)
                }
            };
            if removed {
                self.cursor_col = new_col;
                return true;
            }
            if col > 0 {
                return false;
            }
        }
        if self.cursor_row == 0 {
            return false;
        }
        let original_row = self.cursor_row;
        let current = self.lines.remove(self.cursor_row);
        let prev_idx = self.cursor_row.saturating_sub(1);
        let prev_len = Self::line_len(&self.lines[prev_idx]);
        let curr_len = Self::line_len(&current);
        if prev_len.saturating_add(curr_len) <= NOTES_MAX_COLS {
            let prev = &mut self.lines[prev_idx];
            for ch in current.chars() {
                let _ = prev.push(ch);
            }
            self.cursor_row = prev_idx;
            self.cursor_col = prev_len;
            return true;
        }
        self.lines.insert(original_row, current);
        self.cursor_row = original_row;
        self.cursor_col = 0;
        false
    }

    fn newline(&mut self) -> bool {
        if self.selection_all {
            let _ = self.clear_all();
        }
        self.selection_all = false;
        if self.lines.len() >= NOTES_MAX_LINES {
            return false;
        }
        let current = self.current_line_mut().clone();
        let mut left = HString::<128>::new();
        let mut right = HString::<128>::new();
        for (idx, ch) in current.chars().enumerate() {
            if idx < self.cursor_col {
                let _ = left.push(ch);
            } else {
                let _ = right.push(ch);
            }
        }
        self.lines[self.cursor_row] = left;
        let insert_at = self.cursor_row + 1;
        self.lines.insert(insert_at, right);
        self.cursor_row = insert_at;
        self.cursor_col = 0;
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
        self.cursor_col = 0;
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

    fn to_text(&self) -> String {
        let mut out = String::new();
        for (idx, line) in self.lines.iter().enumerate() {
            if idx > 0 {
                out.push('\n');
            }
            out.push_str(line.as_str());
        }
        out
    }

    fn load_text(&mut self, text: &str) {
        self.clear_all();
        let _ = self.insert_text(text);
        self.selection_all = false;
        self.scroll = 0;
    }

    fn move_left(&mut self) -> bool {
        self.selection_all = false;
        if self.cursor_col > 0 {
            self.cursor_col = self.cursor_col.saturating_sub(1);
            return true;
        }
        if self.cursor_row > 0 {
            self.cursor_row = self.cursor_row.saturating_sub(1);
            let len = Self::line_len(&self.lines[self.cursor_row]);
            self.cursor_col = len;
            return true;
        }
        false
    }

    fn move_right(&mut self) -> bool {
        self.selection_all = false;
        let len = Self::line_len(&self.lines[self.cursor_row]);
        if self.cursor_col < len {
            self.cursor_col = self.cursor_col.saturating_add(1);
            return true;
        }
        if self.cursor_row + 1 < self.lines.len() {
            self.cursor_row = self.cursor_row.saturating_add(1);
            self.cursor_col = 0;
            return true;
        }
        false
    }

    fn move_word_left(&mut self) -> bool {
        self.selection_all = false;
        if self.lines.is_empty() {
            return false;
        }
        if self.cursor_col == 0 {
            if self.cursor_row == 0 {
                return false;
            }
            self.cursor_row = self.cursor_row.saturating_sub(1);
            self.cursor_col = Self::line_len(&self.lines[self.cursor_row]);
        }
        let mut pos = self.cursor_col;
        if move_cursor_word_left(&self.lines[self.cursor_row], &mut pos) {
            self.cursor_col = pos;
            return true;
        }
        if self.cursor_row == 0 {
            return false;
        }
        self.cursor_row = self.cursor_row.saturating_sub(1);
        self.cursor_col = Self::line_len(&self.lines[self.cursor_row]);
        let mut pos = self.cursor_col;
        let _ = move_cursor_word_left(&self.lines[self.cursor_row], &mut pos);
        self.cursor_col = pos;
        true
    }

    fn move_word_right(&mut self) -> bool {
        self.selection_all = false;
        if self.lines.is_empty() {
            return false;
        }
        let len = Self::line_len(&self.lines[self.cursor_row]);
        if self.cursor_col >= len {
            if self.cursor_row + 1 >= self.lines.len() {
                return false;
            }
            self.cursor_row = self.cursor_row.saturating_add(1);
            self.cursor_col = 0;
        }
        let mut pos = self.cursor_col;
        if move_cursor_word_right(&self.lines[self.cursor_row], &mut pos) {
            self.cursor_col = pos;
            return true;
        }
        if self.cursor_row + 1 >= self.lines.len() {
            return false;
        }
        self.cursor_row = self.cursor_row.saturating_add(1);
        self.cursor_col = 0;
        let mut pos = self.cursor_col;
        let _ = move_cursor_word_right(&self.lines[self.cursor_row], &mut pos);
        self.cursor_col = pos;
        true
    }

    fn delete_prev_word(&mut self) -> bool {
        if self.selection_all {
            return self.clear_all();
        }
        self.selection_all = false;
        let mut merged = false;
        if self.cursor_col == 0 {
            if self.cursor_row == 0 {
                return false;
            }
            let original_row = self.cursor_row;
            let current = self.lines.remove(self.cursor_row);
            let prev_idx = self.cursor_row.saturating_sub(1);
            let prev_len = Self::line_len(&self.lines[prev_idx]);
            let curr_len = Self::line_len(&current);
            if prev_len.saturating_add(curr_len) <= NOTES_MAX_COLS {
                let prev = &mut self.lines[prev_idx];
                for ch in current.chars() {
                    let _ = prev.push(ch);
                }
                self.cursor_row = prev_idx;
                self.cursor_col = prev_len;
                merged = true;
            } else {
                self.lines.insert(original_row, current);
                self.cursor_row = original_row;
                self.cursor_col = 0;
                return false;
            }
        }
        let mut pos = self.cursor_col;
        let changed = {
            let line = self.current_line_mut();
            delete_prev_word(line, &mut pos)
        };
        if changed {
            self.cursor_col = pos;
            return true;
        }
        merged
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

struct NotesLayout {
    area: ContentArea,
    view_rows: usize,
    text_cols: usize,
    scrollbar_col: Option<usize>,
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum NotesDialogMode {
    Open,
    Save,
}

struct NotesDialogLayout {
    area: ContentArea,
    list_row: usize,
    list_rows: usize,
    text_cols: usize,
    scrollbar_col: Option<usize>,
    footer_row: usize,
}

struct NotesDialog {
    mode: NotesDialogMode,
    path: Vec<String>,
    entries: Vec<FileEntry>,
    selection: usize,
    scroll: usize,
    input_name: HString<64>,
    status: HString<64>,
}

impl NotesDialog {
    fn new(mode: NotesDialogMode, path: Vec<String>, file_name: Option<String>) -> Self {
        let mut input_name = HString::<64>::new();
        if matches!(mode, NotesDialogMode::Save) {
            if let Some(name) = file_name {
                let _ = input_name.push_str(&name);
            }
        }
        Self {
            mode,
            path,
            entries: Vec::new(),
            selection: 0,
            scroll: 0,
            input_name,
            status: HString::new(),
        }
    }

    fn current_path(&self) -> String {
        if self.path.is_empty() {
            String::from("\\")
        } else {
            let mut out = String::from("\\");
            for (idx, part) in self.path.iter().enumerate() {
                if idx > 0 {
                    out.push('\\');
                }
                out.push_str(part);
            }
            out
        }
    }

    fn join_path(&self, name: &str) -> String {
        if self.path.is_empty() {
            let mut out = String::from("\\");
            out.push_str(name);
            out
        } else {
            let mut out = String::from("\\");
            for part in &self.path {
                out.push_str(part);
                out.push('\\');
            }
            out.push_str(name);
            out
        }
    }

    fn set_status(&mut self, msg: &str) {
        self.status.clear();
        let _ = self.status.push_str(msg);
    }

    fn clear_status(&mut self) {
        self.status.clear();
    }

    fn refresh_entries(&mut self) {
        let mut entries: Vec<FileEntry> = Vec::new();
        if !self.path.is_empty() {
            entries.push(FileEntry {
                name: String::from(".."),
                size: 0,
                kind: FileEntryKind::Parent,
            });
        }

        let path = self.current_path();
        match fs::list_dir(&path) {
            Ok(mut list) => {
                list.sort_by(|a, b| {
                    match (a.is_dir, b.is_dir) {
                        (true, false) => Ordering::Less,
                        (false, true) => Ordering::Greater,
                        _ => a.name.cmp(&b.name),
                    }
                });
                for item in list {
                    entries.push(FileEntry {
                        name: item.name,
                        size: item.size,
                        kind: if item.is_dir { FileEntryKind::Dir } else { FileEntryKind::File },
                    });
                }
                self.clear_status();
            }
            Err(err) => {
                self.set_status(err);
            }
        }

        self.entries = entries;
        if self.selection >= self.entries.len() {
            self.selection = self.entries.len().saturating_sub(1);
        }
        self.scroll = 0;
    }

    fn layout(&self, ctx: &AppContext, total_entries: usize) -> Option<NotesDialogLayout> {
        let area = ctx.metrics.content_area?;
        if area.h < 3 {
            return None;
        }
        let list_rows = area.h.saturating_sub(2);
        let needs_scroll = total_entries > list_rows;
        let reserved = if needs_scroll { SCROLLBAR_COLS + SCROLLBAR_GAP_COLS } else { 0 };
        if area.w <= reserved {
            return None;
        }
        let text_cols = area.w.saturating_sub(reserved);
        let scrollbar_col = if needs_scroll {
            Some(text_cols.saturating_add(SCROLLBAR_GAP_COLS))
        } else {
            None
        };
        Some(NotesDialogLayout {
            area,
            list_row: 1,
            list_rows,
            text_cols,
            scrollbar_col,
            footer_row: area.h.saturating_sub(1),
        })
    }

    fn max_scroll(&self, view_rows: usize) -> usize {
        if view_rows == 0 {
            return 0;
        }
        self.entries.len().saturating_sub(view_rows)
    }

    fn clamp_scroll(&mut self, view_rows: usize) {
        let max_scroll = self.max_scroll(view_rows);
        if self.scroll > max_scroll {
            self.scroll = max_scroll;
        }
        if self.selection >= self.entries.len() {
            self.selection = self.entries.len().saturating_sub(1);
        }
    }

    fn ensure_selection_visible(&mut self, view_rows: usize) {
        if view_rows == 0 {
            return;
        }
        self.clamp_scroll(view_rows);
        if self.selection < self.scroll {
            self.scroll = self.selection;
        } else if self.selection >= self.scroll.saturating_add(view_rows) {
            self.scroll = self.selection.saturating_sub(view_rows - 1);
        }
    }

    fn move_selection(&mut self, delta: i32, view_rows: usize) -> bool {
        if self.entries.is_empty() {
            return false;
        }
        let len = self.entries.len() as i32;
        let mut next = self.selection as i32 + delta;
        if next < 0 {
            next = 0;
        }
        if next >= len {
            next = len - 1;
        }
        let next = next as usize;
        if next == self.selection {
            return false;
        }
        self.selection = next;
        self.ensure_selection_visible(view_rows);
        true
    }
}

struct NotesApp {
    notes: NotesBuffer,
    file_path: Option<String>,
    status: HString<64>,
    dialog: Option<NotesDialog>,
    dirty: bool,
}

impl NotesApp {
    fn new() -> Self {
        Self {
            notes: NotesBuffer::new(),
            file_path: None,
            status: HString::new(),
            dialog: None,
            dirty: false,
        }
    }

    fn status_rows(&self) -> usize {
        if self.dialog.is_some() {
            return 0;
        }
        if !self.status.is_empty() || self.dirty {
            1
        } else {
            0
        }
    }

    fn view_rows(&self, ctx: &AppContext) -> usize {
        let Some(area) = ctx.metrics.content_area else { return 0; };
        area.h.saturating_sub(self.status_rows())
    }

    fn set_status(&mut self, msg: &str) {
        self.status.clear();
        let _ = self.status.push_str(msg);
    }

    fn clear_status(&mut self) {
        self.status.clear();
    }

    fn save_to_path(&mut self, path: &str) -> bool {
        let text = self.notes.to_text();
        match fs::write_file(path, &text) {
            Ok(_) => {
                self.file_path = Some(String::from(path));
                self.dirty = false;
                self.set_status("Saved.");
                true
            }
            Err(err) => {
                self.set_status(err);
                false
            }
        }
    }

    fn load_from_path(&mut self, path: &str) -> bool {
        let Some(contents) = fs::read_file(path) else {
            self.set_status("File not found.");
            return false;
        };
        self.notes.load_text(&contents);
        self.file_path = Some(String::from(path));
        self.dirty = false;
        self.set_status("Loaded.");
        true
    }

    fn split_path(path: &str) -> (Vec<String>, Option<String>) {
        let trimmed = path.trim_matches('\\');
        if trimmed.is_empty() {
            return (Vec::new(), None);
        }
        let mut parts: Vec<String> = trimmed
            .split('\\')
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();
        let file = parts.pop();
        (parts, file)
    }

    fn open_dialog(&mut self, mode: NotesDialogMode) {
        let (path, file) = self
            .file_path
            .as_deref()
            .map(Self::split_path)
            .unwrap_or((Vec::new(), None));
        let mut dialog = NotesDialog::new(mode, path, file);
        dialog.refresh_entries();
        self.dialog = Some(dialog);
    }

    fn dialog_scrollbar(
        &self,
        ctx: &AppContext,
        dialog: &NotesDialog,
        layout: &NotesDialogLayout,
    ) -> Option<ScrollbarDraw> {
        if dialog.entries.len() <= layout.list_rows {
            return None;
        }
        let scrollbar_col = layout.scrollbar_col?;
        let char_w = ctx.metrics.char_w.max(1);
        let char_h = ctx.metrics.char_h.max(1);
        if char_w == 0 || char_h == 0 {
            return None;
        }
        let track = Rect {
            x: layout.area.x.saturating_add(scrollbar_col).saturating_mul(char_w),
            y: layout.area.y.saturating_add(layout.list_row).saturating_mul(char_h),
            w: char_w,
            h: layout.list_rows.saturating_mul(char_h),
        };
        let max_scroll = dialog.entries.len().saturating_sub(layout.list_rows);
        let thumb_h = (track.h.saturating_mul(layout.list_rows) / dialog.entries.len())
            .max(char_h)
            .min(track.h);
        let available = track.h.saturating_sub(thumb_h);
        let scroll = dialog.scroll.min(max_scroll);
        let thumb_y = if max_scroll == 0 {
            track.y
        } else {
            track.y.saturating_add(available.saturating_mul(scroll) / max_scroll)
        };
        let thumb = Rect { x: track.x, y: thumb_y, w: track.w, h: thumb_h };
        Some(ScrollbarDraw { track, thumb })
    }

    fn draw_dialog(&self, ctx: &mut AppContext, dialog: &NotesDialog) {
        let total_entries = dialog.entries.len();
        let Some(layout) = dialog.layout(ctx, total_entries) else { return; };
        let char_w = ctx.metrics.char_w.max(1);
        let char_h = ctx.metrics.char_h.max(1);

        let header_bg = apply_intensity(ctx.colors.fg, ctx.colors.bg, 20);
        let header_y = layout.area.y;
        ctx.fill_rect(
            layout.area.x.saturating_mul(char_w),
            header_y.saturating_mul(char_h),
            layout.area.w.saturating_mul(char_w),
            char_h,
            header_bg,
        );

        let mut header = HString::<128>::new();
        let _ = header.push_str(dialog.current_path().as_str());
        if !dialog.status.is_empty() {
            let _ = header.push_str(" - ");
            let _ = header.push_str(dialog.status.as_str());
        }
        let mut header_trimmed = HString::<128>::new();
        for ch in header.chars().take(layout.text_cols) {
            let _ = header_trimmed.push(ch);
        }
        ctx.draw_text_at_char(layout.area.x, header_y, header_trimmed.as_str(), ctx.colors.fg, header_bg);

        let highlight = apply_intensity(ctx.colors.fg, ctx.colors.bg, 48);
        let start = dialog.scroll.min(total_entries.saturating_sub(layout.list_rows));
        for (row, entry) in dialog.entries.iter().skip(start).take(layout.list_rows).enumerate() {
            let list_row = layout.list_row.saturating_add(row);
            let abs_row = layout.area.y.saturating_add(list_row);
            let selected = start.saturating_add(row) == dialog.selection;
            let row_bg = if selected { highlight } else { ctx.colors.bg };
            if selected {
                ctx.fill_rect(
                    layout.area.x.saturating_mul(char_w),
                    abs_row.saturating_mul(char_h),
                    layout.area.w.saturating_mul(char_w),
                    char_h,
                    highlight,
                );
            }
            let mut name = HString::<128>::new();
            let mut label = entry.name.as_str();
            if matches!(entry.kind, FileEntryKind::Parent) {
                label = "..";
            }
            for ch in label.chars().take(layout.text_cols.saturating_sub(1)) {
                let _ = name.push(ch);
            }
            if matches!(entry.kind, FileEntryKind::Dir | FileEntryKind::Parent) {
                let _ = name.push('\\');
            }
            ctx.draw_text_at_char(layout.area.x, abs_row, name.as_str(), ctx.colors.fg, row_bg);
        }

        let footer_bg = apply_intensity(ctx.colors.fg, ctx.colors.bg, 20);
        let footer_row = layout.area.y.saturating_add(layout.footer_row);
        ctx.fill_rect(
            layout.area.x.saturating_mul(char_w),
            footer_row.saturating_mul(char_h),
            layout.area.w.saturating_mul(char_w),
            char_h,
            footer_bg,
        );
        let mut footer = HString::<128>::new();
        match dialog.mode {
            NotesDialogMode::Open => {
                let _ = footer.push_str("Enter to open");
            }
            NotesDialogMode::Save => {
                let _ = footer.push_str("Name: ");
                let _ = footer.push_str(dialog.input_name.as_str());
            }
        }
        let mut footer_trimmed = HString::<128>::new();
        for ch in footer.chars().take(layout.text_cols) {
            let _ = footer_trimmed.push(ch);
        }
        ctx.draw_text_at_char(layout.area.x, footer_row, footer_trimmed.as_str(), ctx.colors.fg, footer_bg);

        if let Some(scrollbar) = self.dialog_scrollbar(ctx, dialog, &layout) {
            ctx.draw_scrollbar(scrollbar.track, scrollbar.thumb);
        }
    }

    fn handle_dialog_key(&mut self, ctx: &AppContext, evt: &KeyEvent) -> AppEventResult {
        enum DialogAction {
            Open(String),
            Save(String),
        }

        let pending = {
            let Some(dialog) = self.dialog.as_mut() else { return AppEventResult::Ignored; };
            let total_entries = dialog.entries.len();
            let Some(layout) = dialog.layout(ctx, total_entries) else { return AppEventResult::Ignored; };
            let view_rows = layout.list_rows;

            match evt {
                KeyEvent::Up => {
                    if dialog.move_selection(-1, view_rows) {
                        return AppEventResult::HandledRedraw;
                    }
                    return AppEventResult::HandledNoRedraw;
                }
                KeyEvent::Down => {
                    if dialog.move_selection(1, view_rows) {
                        return AppEventResult::HandledRedraw;
                    }
                    return AppEventResult::HandledNoRedraw;
                }
                KeyEvent::Backspace => {
                    if matches!(dialog.mode, NotesDialogMode::Save) && !dialog.input_name.is_empty() {
                        dialog.input_name.pop();
                        return AppEventResult::HandledRedraw;
                    }
                    if !dialog.path.is_empty() {
                        dialog.path.pop();
                        dialog.refresh_entries();
                        dialog.ensure_selection_visible(view_rows);
                        return AppEventResult::HandledRedraw;
                    }
                    return AppEventResult::HandledNoRedraw;
                }
                KeyEvent::Enter => {
                    let Some(entry) = dialog.entries.get(dialog.selection) else {
                        return AppEventResult::HandledNoRedraw;
                    };
                    let entry_kind = entry.kind;
                    let entry_name = entry.name.clone();
                    match dialog.mode {
                        NotesDialogMode::Open => match entry_kind {
                            FileEntryKind::Parent => {
                                if !dialog.path.is_empty() {
                                    dialog.path.pop();
                                    dialog.refresh_entries();
                                    dialog.ensure_selection_visible(view_rows);
                                }
                                return AppEventResult::HandledRedraw;
                            }
                            FileEntryKind::Dir => {
                                dialog.path.push(entry_name);
                                dialog.refresh_entries();
                                dialog.ensure_selection_visible(view_rows);
                                return AppEventResult::HandledRedraw;
                            }
                            FileEntryKind::File => {
                                let path = dialog.join_path(&entry_name);
                                Some(DialogAction::Open(path))
                            }
                        },
                        NotesDialogMode::Save => {
                            if !dialog.input_name.is_empty() {
                                let path = dialog.join_path(dialog.input_name.as_str());
                                Some(DialogAction::Save(path))
                            } else {
                                match entry_kind {
                                    FileEntryKind::Parent => {
                                        if !dialog.path.is_empty() {
                                            dialog.path.pop();
                                            dialog.refresh_entries();
                                            dialog.ensure_selection_visible(view_rows);
                                        }
                                        return AppEventResult::HandledRedraw;
                                    }
                                    FileEntryKind::Dir => {
                                        dialog.path.push(entry_name);
                                        dialog.refresh_entries();
                                        dialog.ensure_selection_visible(view_rows);
                                        return AppEventResult::HandledRedraw;
                                    }
                                    FileEntryKind::File => {
                                        let path = dialog.join_path(&entry_name);
                                        Some(DialogAction::Save(path))
                                    }
                                }
                            }
                        }
                    }
                }
                &KeyEvent::Char(ch) => {
                    if matches!(dialog.mode, NotesDialogMode::Save) {
                        if ch != '\\' && ch != '/' {
                            let _ = dialog.input_name.push(ch);
                            return AppEventResult::HandledRedraw;
                        }
                    }
                    return AppEventResult::HandledNoRedraw;
                }
                _ => return AppEventResult::HandledNoRedraw,
            }
        };

        let Some(action) = pending else { return AppEventResult::HandledNoRedraw; };
        let ok = match action {
            DialogAction::Open(path) => self.load_from_path(&path),
            DialogAction::Save(path) => self.save_to_path(&path),
        };
        if ok {
            self.dialog = None;
            return AppEventResult::HandledRedraw;
        }
        if let Some(dialog) = self.dialog.as_mut() {
            dialog.set_status(self.status.as_str());
        }
        AppEventResult::HandledRedraw
    }

    fn layout(&self, ctx: &AppContext, total_lines: usize) -> Option<NotesLayout> {
        let area = ctx.metrics.content_area?;
        let status_rows = self.status_rows();
        let view_rows = area.h.saturating_sub(status_rows);
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

    fn scrollbar_draw(&self, ctx: &AppContext, layout: &NotesLayout, total_lines: usize) -> Option<ScrollbarDraw> {
        if total_lines <= layout.view_rows {
            return None;
        }
        let char_w = ctx.metrics.char_w.max(1);
        let char_h = ctx.metrics.char_h.max(1);
        let scrollbar_col = layout.scrollbar_col?;
        if char_h == 0 {
            return None;
        }
        let track = Rect {
            x: scrollbar_col.saturating_mul(char_w),
            y: layout.area.y.saturating_mul(char_h),
            w: char_w,
            h: layout.view_rows.saturating_mul(char_h),
        };
        let max_scroll = total_lines.saturating_sub(layout.view_rows);
        let thumb_h = (track.h.saturating_mul(layout.view_rows) / total_lines)
            .max(char_h)
            .min(track.h);
        let available = track.h.saturating_sub(thumb_h);
        let scroll = self.notes.scroll.min(max_scroll);
        let thumb_y = if max_scroll == 0 {
            track.y
        } else {
            track.y.saturating_add(available.saturating_mul(scroll) / max_scroll)
        };
        let thumb = Rect { x: track.x, y: thumb_y, w: track.w, h: thumb_h };
        Some(ScrollbarDraw { track, thumb })
    }
}

impl WindowApp for NotesApp {
    fn accepts_input(&self) -> bool {
        true
    }

    fn draw(&mut self, ctx: &mut AppContext, input_focus: bool) {
        if let Some(dialog) = &self.dialog {
            self.draw_dialog(ctx, dialog);
            return;
        }
        let total_lines = self.notes.lines.len();
        let layout = self.layout(ctx, total_lines);
        let Some(layout) = layout else { return; };
        let view_rows = layout.view_rows;
        let text_cols = layout.text_cols;
        let start_row = layout.area.y;
        let start_col = layout.area.x;
        let max_start = total_lines.saturating_sub(view_rows);
        let start_line = self.notes.scroll.min(max_start);
        let char_w = ctx.metrics.char_w.max(1);
        let char_h = ctx.metrics.char_h.max(1);
        let selection_bg = if self.notes.selection_all {
            Some(apply_intensity(ctx.colors.fg, ctx.colors.bg, 48))
        } else {
            None
        };
        for (i, line) in self.notes.lines.iter().skip(start_line).take(view_rows).enumerate() {
            if text_cols == 0 {
                continue;
            }
            let mut buf = HString::<128>::new();
            for ch in line.chars().take(text_cols) {
                let _ = buf.push(ch);
            }
            let row = start_row + i;
            let bg = selection_bg.unwrap_or(ctx.colors.bg);
            if let Some(bg) = selection_bg {
                ctx.fill_rect(
                    start_col.saturating_mul(char_w),
                    row.saturating_mul(char_h),
                    text_cols.saturating_mul(char_w),
                    char_h,
                    bg,
                );
            }
            ctx.draw_text_at_char(start_col, row, buf.as_str(), ctx.colors.fg, bg);
        }
        if input_focus && total_lines > 0 {
            let cursor_row = self.notes.cursor_row.min(total_lines.saturating_sub(1));
            if cursor_row >= start_line {
                let row = start_row + cursor_row - start_line;
                if row < start_row.saturating_add(view_rows) {
                    let line = &self.notes.lines[cursor_row];
                    let line_len = line.chars().count();
                    let col = self.notes.cursor_col.min(line_len).min(text_cols.saturating_sub(1));
                    if text_cols > 0 {
                        ctx.draw_text_cursor(start_col + col, row, ctx.colors.fg);
                    }
                }
            }
        }
        if let Some(scrollbar) = self.scrollbar_draw(ctx, &layout, total_lines) {
            ctx.draw_scrollbar(scrollbar.track, scrollbar.thumb);
        }

        let status_rows = self.status_rows();
        if status_rows > 0 && layout.view_rows < layout.area.h {
            let char_w = ctx.metrics.char_w.max(1);
            let char_h = ctx.metrics.char_h.max(1);
            if char_h > 0 && char_w > 0 {
                let status_bg = apply_intensity(ctx.colors.fg, ctx.colors.bg, 20);
                let row = layout.area.y.saturating_add(layout.view_rows);
                ctx.fill_rect(
                    layout.area.x.saturating_mul(char_w),
                    row.saturating_mul(char_h),
                    layout.area.w.saturating_mul(char_w),
                    char_h,
                    status_bg,
                );
                let mut trimmed = HString::<128>::new();
                let message = if self.status.is_empty() {
                    "Unsaved changes"
                } else {
                    self.status.as_str()
                };
                for ch in message.chars().take(layout.text_cols) {
                    let _ = trimmed.push(ch);
                }
                ctx.draw_text_at_char(layout.area.x, row, trimmed.as_str(), ctx.colors.fg, status_bg);
            }
        }
    }

    fn handle_key(&mut self, ctx: &mut AppContext, evt: &KeyEvent) -> AppEventResult {
        if self.dialog.is_some() {
            return self.handle_dialog_key(ctx, evt);
        }

        let view_rows = self.view_rows(ctx);
        let mut handled = false;
        let mut changed = false;
        let mut moved = false;
        match evt {
            &KeyEvent::Char(ch) => {
                handled = true;
                changed = self.notes.push_char(ch) || changed;
            }
            &KeyEvent::Backspace => {
                handled = true;
                changed = self.notes.backspace() || changed;
            }
            &KeyEvent::CtrlBackspace => {
                handled = true;
                changed = self.notes.delete_prev_word() || changed;
            }
            &KeyEvent::Enter => {
                handled = true;
                changed = self.notes.newline() || changed;
            }
            &KeyEvent::Tab => {
                handled = true;
                for _ in 0..4 {
                    changed = self.notes.push_char(' ') || changed;
                }
            }
            KeyEvent::CtrlA => {
                handled = true;
                self.notes.select_all();
            }
            KeyEvent::CtrlC => {
                handled = true;
                if let Some(text) = self.notes.selection_text() {
                    clipboard::set_text(&text);
                }
            }
            KeyEvent::CtrlX => {
                handled = true;
                if let Some(text) = self.notes.selection_text() {
                    clipboard::set_text(&text);
                    changed = self.notes.clear_all() || changed;
                }
            }
            KeyEvent::CtrlV => {
                handled = true;
                let clip_text = clipboard::get_text();
                if !clip_text.is_empty() {
                    if self.notes.selection_all {
                        changed = self.notes.clear_all() || changed;
                    }
                    changed = self.notes.insert_text(&clip_text) || changed;
                }
            }
            KeyEvent::Left | KeyEvent::ShiftLeft => {
                handled = true;
                moved = self.notes.move_left() || moved;
            }
            KeyEvent::Right | KeyEvent::ShiftRight => {
                handled = true;
                moved = self.notes.move_right() || moved;
            }
            KeyEvent::CtrlLeft | KeyEvent::CtrlShiftLeft => {
                handled = true;
                moved = self.notes.move_word_left() || moved;
            }
            KeyEvent::CtrlRight | KeyEvent::CtrlShiftRight => {
                handled = true;
                moved = self.notes.move_word_right() || moved;
            }
            KeyEvent::CtrlS => {
                if let Some(path) = self.file_path.clone() {
                    self.save_to_path(&path);
                    return AppEventResult::HandledRedraw;
                }
                self.open_dialog(NotesDialogMode::Save);
                return AppEventResult::HandledRedraw;
            }
            KeyEvent::CtrlO => {
                self.open_dialog(NotesDialogMode::Open);
                return AppEventResult::HandledRedraw;
            }
            _ => {}
        }

        if handled {
            if changed || moved {
                self.notes.ensure_cursor_visible(view_rows);
                if changed {
                    self.dirty = true;
                    self.clear_status();
                }
                return AppEventResult::HandledRedraw;
            }
            return AppEventResult::HandledNoRedraw;
        }
        AppEventResult::Ignored
    }

    fn handle_mouse(&mut self, ctx: &mut AppContext, evt: &MouseEvent) -> AppEventResult {
        if matches!(evt.kind, MouseEventKind::Up) {
            return AppEventResult::Ignored;
        }
        if let Some(dialog) = self.dialog.as_mut() {
            let total_entries = dialog.entries.len();
            let Some(layout) = dialog.layout(ctx, total_entries) else { return AppEventResult::Ignored; };
            if evt.row < layout.list_row || evt.row >= layout.list_row.saturating_add(layout.list_rows) {
                return AppEventResult::HandledNoRedraw;
            }
            let row = evt.row.saturating_sub(layout.list_row);
            let idx = dialog.scroll.saturating_add(row);
            if idx >= dialog.entries.len() {
                return AppEventResult::HandledNoRedraw;
            }
            dialog.selection = idx;
            dialog.ensure_selection_visible(layout.list_rows);
            if evt.clicks >= 2 {
                return self.handle_dialog_key(ctx, &KeyEvent::Enter);
            }
            return AppEventResult::HandledRedraw;
        }

        let total_lines = self.notes.lines.len();
        let Some(layout) = self.layout(ctx, total_lines) else { return AppEventResult::Ignored; };
        if evt.row >= layout.view_rows {
            return AppEventResult::HandledNoRedraw;
        }
        if evt.col >= layout.text_cols {
            return AppEventResult::HandledNoRedraw;
        }
        let idx = self.notes.scroll.saturating_add(evt.row);
        if total_lines == 0 {
            return AppEventResult::HandledNoRedraw;
        }
        let idx = idx.min(total_lines.saturating_sub(1));
        let line_len = self.notes.lines[idx].chars().count();
        let col = evt.col.min(line_len);
        let moved = self.notes.set_cursor(idx, col);
        self.notes.ensure_cursor_visible(layout.view_rows);
        if moved {
            AppEventResult::HandledRedraw
        } else {
            AppEventResult::HandledNoRedraw
        }
    }

    fn scroll_by(&mut self, ctx: &mut AppContext, lines: i32) -> bool {
        if lines == 0 {
            return false;
        }
        if let Some(dialog) = self.dialog.as_mut() {
            let total_entries = dialog.entries.len();
            let Some(layout) = dialog.layout(ctx, total_entries) else { return false; };
            let max_scroll = dialog.max_scroll(layout.list_rows) as i32;
            let mut next = dialog.scroll as i32 + lines;
            if next < 0 {
                next = 0;
            }
            if next > max_scroll {
                next = max_scroll;
            }
            let next = next as usize;
            if next == dialog.scroll {
                return false;
            }
            dialog.scroll = next;
            return true;
        }
        let view_rows = self.view_rows(ctx);
        self.notes.scroll_by(lines, view_rows)
    }

    fn scroll_to(&mut self, ctx: &mut AppContext, scroll: usize) -> bool {
        if let Some(dialog) = self.dialog.as_mut() {
            let total_entries = dialog.entries.len();
            let Some(layout) = dialog.layout(ctx, total_entries) else { return false; };
            let max_scroll = dialog.max_scroll(layout.list_rows);
            let scroll = scroll.min(max_scroll);
            if scroll == dialog.scroll {
                return false;
            }
            dialog.scroll = scroll;
            return true;
        }
        let view_rows = self.view_rows(ctx);
        let max_scroll = self.notes.max_scroll(view_rows);
        let scroll = scroll.min(max_scroll);
        if scroll == self.notes.scroll {
            return false;
        }
        self.notes.scroll = scroll;
        true
    }

    fn scroll_metrics(&self, ctx: &AppContext) -> Option<ScrollMetrics> {
        if let Some(dialog) = &self.dialog {
            let layout = dialog.layout(ctx, dialog.entries.len())?;
            if let Some(scrollbar) = self.dialog_scrollbar(ctx, dialog, &layout) {
                return Some(ScrollMetrics {
                    track: scrollbar.track,
                    thumb_h: scrollbar.thumb.h,
                    max_scroll: dialog.entries.len().saturating_sub(layout.list_rows),
                    scroll: dialog.scroll,
                });
            }
            return None;
        }
        let layout = self.layout(ctx, self.notes.lines.len())?;
        if self.notes.lines.len() <= layout.view_rows {
            return None;
        }
        let scrollbar_col = layout.scrollbar_col?;
        let char_w = ctx.metrics.char_w.max(1);
        let char_h = ctx.metrics.char_h.max(1);
        let track = Rect {
            x: ctx.metrics.x.saturating_add(scrollbar_col.saturating_mul(char_w)),
            y: ctx.metrics.y.saturating_add(layout.area.y.saturating_mul(char_h)),
            w: char_w,
            h: layout.view_rows.saturating_mul(char_h),
        };
        let max_scroll = self.notes.lines.len().saturating_sub(layout.view_rows);
        let thumb_h = (track.h.saturating_mul(layout.view_rows) / self.notes.lines.len())
            .max(char_h)
            .min(track.h);
        let scroll = self.notes.scroll.min(max_scroll);
        Some(ScrollMetrics { track, thumb_h, max_scroll, scroll })
    }

    fn on_resize(&mut self, ctx: &AppContext) {
        if let Some(dialog) = &mut self.dialog {
            if let Some(layout) = dialog.layout(ctx, dialog.entries.len()) {
                dialog.clamp_scroll(layout.list_rows);
            }
            return;
        }
        let view_rows = self.view_rows(ctx);
        self.notes.clamp_scroll(view_rows);
    }

    fn open_path(&mut self, path: &str) -> bool {
        self.dialog = None;
        self.load_from_path(path)
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum FileEntryKind {
    Parent,
    Dir,
    File,
}

struct FileEntry {
    name: String,
    size: usize,
    kind: FileEntryKind,
}

struct FileExplorerLayout {
    area: ContentArea,
    list_row: usize,
    list_rows: usize,
    text_cols: usize,
    scrollbar_col: Option<usize>,
}

struct AppPickerState {
    apps: Vec<usize>,
    selection: usize,
    scroll: usize,
    file_path: String,
}

struct AppPickerLayout {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    list_row: usize,
    list_rows: usize,
    text_col: usize,
    text_cols: usize,
    scrollbar_col: Option<usize>,
}

struct FileExplorerApp {
    path: Vec<String>,
    entries: Vec<FileEntry>,
    selection: usize,
    scroll: usize,
    status: HString<64>,
    app_picker: Option<AppPickerState>,
    pending_action: Option<AppAction>,
}

impl FileExplorerApp {
    fn new() -> Self {
        let mut app = Self {
            path: Vec::new(),
            entries: Vec::new(),
            selection: 0,
            scroll: 0,
            status: HString::new(),
            app_picker: None,
            pending_action: None,
        };
        app.refresh_entries();
        app
    }

    fn current_path(&self) -> String {
        if self.path.is_empty() {
            String::from("\\")
        } else {
            let mut out = String::from("\\");
            for (idx, part) in self.path.iter().enumerate() {
                if idx > 0 {
                    out.push('\\');
                }
                out.push_str(part);
            }
            out
        }
    }

    fn join_path(&self, name: &str) -> String {
        if self.path.is_empty() {
            let mut out = String::from("\\");
            out.push_str(name);
            out
        } else {
            let mut out = String::from("\\");
            for part in &self.path {
                out.push_str(part);
                out.push('\\');
            }
            out.push_str(name);
            out
        }
    }

    fn set_status(&mut self, msg: &str) {
        self.status.clear();
        let _ = self.status.push_str(msg);
    }

    fn clear_status(&mut self) {
        self.status.clear();
    }

    fn refresh_entries(&mut self) {
        let mut entries: Vec<FileEntry> = Vec::new();
        if !self.path.is_empty() {
            entries.push(FileEntry {
                name: String::from(".."),
                size: 0,
                kind: FileEntryKind::Parent,
            });
        }

        let path = self.current_path();
        match fs::list_dir(&path) {
            Ok(mut list) => {
                list.sort_by(|a, b| {
                    match (a.is_dir, b.is_dir) {
                        (true, false) => Ordering::Less,
                        (false, true) => Ordering::Greater,
                        _ => a.name.cmp(&b.name),
                    }
                });
                for item in list {
                    entries.push(FileEntry {
                        name: item.name,
                        size: item.size,
                        kind: if item.is_dir { FileEntryKind::Dir } else { FileEntryKind::File },
                    });
                }
                self.clear_status();
            }
            Err(err) => {
                self.set_status(err);
            }
        }

        self.entries = entries;
        if self.selection >= self.entries.len() {
            self.selection = self.entries.len().saturating_sub(1);
        }
        self.scroll = 0;
    }

    fn layout(&self, ctx: &AppContext, total_entries: usize) -> Option<FileExplorerLayout> {
        let area = ctx.metrics.content_area?;
        if area.h <= FILE_EXPLORER_HEADER_ROWS {
            return None;
        }
        let list_rows = area.h.saturating_sub(FILE_EXPLORER_HEADER_ROWS);
        let needs_scroll = total_entries > list_rows;
        let reserved = if needs_scroll { SCROLLBAR_COLS + SCROLLBAR_GAP_COLS } else { 0 };
        if area.w <= reserved {
            return None;
        }
        let text_cols = area.w.saturating_sub(reserved);
        let scrollbar_col = if needs_scroll {
            Some(text_cols.saturating_add(SCROLLBAR_GAP_COLS))
        } else {
            None
        };
        Some(FileExplorerLayout {
            area,
            list_row: FILE_EXPLORER_HEADER_ROWS,
            list_rows,
            text_cols,
            scrollbar_col,
        })
    }

    fn name_and_size_cols(&self, text_cols: usize) -> (usize, Option<usize>) {
        let mut name_cols = text_cols;
        let mut size_col = None;
        let needed = FILE_EXPLORER_SIZE_COLS
            .saturating_add(FILE_EXPLORER_SIZE_GAP)
            .saturating_add(4);
        if text_cols >= needed {
            let col = text_cols.saturating_sub(FILE_EXPLORER_SIZE_COLS);
            size_col = Some(col);
            name_cols = col.saturating_sub(FILE_EXPLORER_SIZE_GAP).max(1);
        }
        (name_cols.max(1), size_col)
    }

    fn max_scroll(&self, view_rows: usize) -> usize {
        if view_rows == 0 {
            return 0;
        }
        self.entries.len().saturating_sub(view_rows)
    }

    fn clamp_scroll(&mut self, view_rows: usize) {
        let max_scroll = self.max_scroll(view_rows);
        if self.scroll > max_scroll {
            self.scroll = max_scroll;
        }
        if self.selection >= self.entries.len() {
            self.selection = self.entries.len().saturating_sub(1);
        }
    }

    fn ensure_selection_visible(&mut self, view_rows: usize) {
        if view_rows == 0 {
            return;
        }
        self.clamp_scroll(view_rows);
        if self.selection < self.scroll {
            self.scroll = self.selection;
        } else if self.selection >= self.scroll.saturating_add(view_rows) {
            self.scroll = self.selection.saturating_sub(view_rows - 1);
        }
    }

    fn move_selection(&mut self, delta: i32, view_rows: usize) -> bool {
        if self.entries.is_empty() {
            return false;
        }
        let len = self.entries.len() as i32;
        let mut next = self.selection as i32 + delta;
        if next < 0 {
            next = 0;
        }
        if next >= len {
            next = len - 1;
        }
        let next = next as usize;
        if next == self.selection {
            return false;
        }
        self.selection = next;
        self.ensure_selection_visible(view_rows);
        true
    }

    fn open_selected(&mut self, view_rows: usize) -> bool {
        let Some(entry) = self.entries.get(self.selection) else { return false; };
        match entry.kind {
            FileEntryKind::Parent => {
                if !self.path.is_empty() {
                    self.path.pop();
                    self.refresh_entries();
                    self.ensure_selection_visible(view_rows);
                }
                true
            }
            FileEntryKind::Dir => {
                self.path.push(entry.name.clone());
                self.refresh_entries();
                self.ensure_selection_visible(view_rows);
                true
            }
            FileEntryKind::File => {
                let file_path = self.join_path(&entry.name);
                self.open_app_picker(file_path);
                true
            }
        }
    }

    fn open_app_picker(&mut self, file_path: String) {
        let apps: Vec<usize> = builtin_apps()
            .iter()
            .enumerate()
            .filter_map(|(idx, app)| if app.openable { Some(idx) } else { None })
            .collect();
        if apps.is_empty() {
            self.set_status("No apps available.");
            return;
        }
        self.app_picker = Some(AppPickerState {
            apps,
            selection: 0,
            scroll: 0,
            file_path,
        });
    }

    fn app_picker_layout(&self, ctx: &AppContext, app_count: usize) -> Option<AppPickerLayout> {
        let area = ctx.metrics.content_area?;
        if area.w < APP_PICKER_MIN_COLS || area.h < APP_PICKER_MIN_ROWS {
            return None;
        }
        let max_label = builtin_apps()
            .iter()
            .map(|app| app.label.chars().count())
            .max()
            .unwrap_or(0);
        let mut cols = max_label.saturating_add(4);
        cols = cols.max(APP_PICKER_MIN_COLS).min(APP_PICKER_MAX_COLS).min(area.w);

        let list_rows = app_count.max(1).min(APP_PICKER_MAX_ROWS.saturating_sub(1));
        let mut rows = list_rows.saturating_add(1);
        rows = rows.max(APP_PICKER_MIN_ROWS).min(APP_PICKER_MAX_ROWS).min(area.h);
        let list_rows = rows.saturating_sub(1).max(1);

        let needs_scroll = app_count > list_rows;
        let reserved = if needs_scroll { SCROLLBAR_COLS + SCROLLBAR_GAP_COLS } else { 0 };
        let inner_cols = cols.saturating_sub(2).max(1);
        if inner_cols <= reserved {
            return None;
        }
        let text_cols = inner_cols.saturating_sub(reserved);
        let text_col = 1usize;
        let scrollbar_col = if needs_scroll {
            Some(text_col.saturating_add(text_cols).saturating_add(SCROLLBAR_GAP_COLS))
        } else {
            None
        };

        let x = (area.w.saturating_sub(cols)) / 2;
        let y = (area.h.saturating_sub(rows)) / 2;

        Some(AppPickerLayout {
            x,
            y,
            w: cols,
            h: rows,
            list_row: 1,
            list_rows,
            text_col,
            text_cols,
            scrollbar_col,
        })
    }

    fn app_picker_move(&mut self, delta: i32, layout: &AppPickerLayout) -> bool {
        let Some(picker) = self.app_picker.as_mut() else { return false; };
        if picker.apps.is_empty() {
            return false;
        }
        let len = picker.apps.len() as i32;
        let mut next = picker.selection as i32 + delta;
        if next < 0 {
            next = 0;
        }
        if next >= len {
            next = len - 1;
        }
        let next = next as usize;
        if next == picker.selection {
            return false;
        }
        picker.selection = next;
        let max_scroll = picker.apps.len().saturating_sub(layout.list_rows);
        if picker.selection < picker.scroll {
            picker.scroll = picker.selection;
        } else if picker.selection >= picker.scroll.saturating_add(layout.list_rows) {
            picker.scroll = picker.selection.saturating_sub(layout.list_rows - 1);
        }
        picker.scroll = picker.scroll.min(max_scroll);
        true
    }

    fn app_picker_open(&mut self) -> bool {
        let Some(picker) = self.app_picker.take() else { return false; };
        let Some(app_idx) = picker.apps.get(picker.selection).copied() else { return false; };
        self.pending_action = Some(AppAction::OpenFile {
            app_idx,
            path: picker.file_path,
        });
        true
    }

    fn scrollbar_draw(
        &self,
        ctx: &AppContext,
        layout: &FileExplorerLayout,
        total_entries: usize,
    ) -> Option<ScrollbarDraw> {
        if total_entries <= layout.list_rows {
            return None;
        }
        let scrollbar_col = layout.scrollbar_col?;
        let char_w = ctx.metrics.char_w.max(1);
        let char_h = ctx.metrics.char_h.max(1);
        if char_w == 0 || char_h == 0 {
            return None;
        }
        let track = Rect {
            x: layout.area.x.saturating_add(scrollbar_col).saturating_mul(char_w),
            y: layout.area.y.saturating_add(layout.list_row).saturating_mul(char_h),
            w: char_w,
            h: layout.list_rows.saturating_mul(char_h),
        };
        let max_scroll = total_entries.saturating_sub(layout.list_rows);
        let thumb_h = (track.h.saturating_mul(layout.list_rows) / total_entries)
            .max(char_h)
            .min(track.h);
        let available = track.h.saturating_sub(thumb_h);
        let scroll = self.scroll.min(max_scroll);
        let thumb_y = if max_scroll == 0 {
            track.y
        } else {
            track.y.saturating_add(available.saturating_mul(scroll) / max_scroll)
        };
        let thumb = Rect { x: track.x, y: thumb_y, w: track.w, h: thumb_h };
        Some(ScrollbarDraw { track, thumb })
    }

    fn app_picker_scrollbar(
        &self,
        ctx: &AppContext,
        picker: &AppPickerState,
        layout: &AppPickerLayout,
    ) -> Option<ScrollbarDraw> {
        if picker.apps.len() <= layout.list_rows {
            return None;
        }
        let scrollbar_col = layout.scrollbar_col?;
        let area = ctx.metrics.content_area?;
        let char_w = ctx.metrics.char_w.max(1);
        let char_h = ctx.metrics.char_h.max(1);
        if char_w == 0 || char_h == 0 {
            return None;
        }
        let track = Rect {
            x: area
                .x
                .saturating_add(layout.x + scrollbar_col)
                .saturating_mul(char_w),
            y: area
                .y
                .saturating_add(layout.y + layout.list_row)
                .saturating_mul(char_h),
            w: char_w,
            h: layout.list_rows.saturating_mul(char_h),
        };
        let max_scroll = picker.apps.len().saturating_sub(layout.list_rows);
        let thumb_h = (track.h.saturating_mul(layout.list_rows) / picker.apps.len())
            .max(char_h)
            .min(track.h);
        let available = track.h.saturating_sub(thumb_h);
        let scroll = picker.scroll.min(max_scroll);
        let thumb_y = if max_scroll == 0 {
            track.y
        } else {
            track.y.saturating_add(available.saturating_mul(scroll) / max_scroll)
        };
        let thumb = Rect { x: track.x, y: thumb_y, w: track.w, h: thumb_h };
        Some(ScrollbarDraw { track, thumb })
    }

    fn format_size(&self, size: usize) -> HString<16> {
        let mut out = HString::<16>::new();
        let _ = write!(&mut out, "{}", size);
        out
    }
}

impl WindowApp for FileExplorerApp {
    fn accepts_input(&self) -> bool {
        true
    }

    fn draw(&mut self, ctx: &mut AppContext, _input_focus: bool) {
        let total_entries = self.entries.len();
        let Some(layout) = self.layout(ctx, total_entries) else { return; };
        let area = layout.area;
        let view_rows = layout.list_rows;
        let (name_cols, size_col) = self.name_and_size_cols(layout.text_cols);
        let char_w = ctx.metrics.char_w.max(1);
        let char_h = ctx.metrics.char_h.max(1);

        let header_bg = apply_intensity(ctx.colors.fg, ctx.colors.bg, 20);
        let header_y = area.y;
        ctx.fill_rect(
            area.x.saturating_mul(char_w),
            header_y.saturating_mul(char_h),
            area.w.saturating_mul(char_w),
            char_h,
            header_bg,
        );

        let mut header = HString::<128>::new();
        let _ = header.push_str(self.current_path().as_str());
        if !self.status.is_empty() {
            let _ = header.push_str(" - ");
            let _ = header.push_str(self.status.as_str());
        }
        let mut header_trimmed = HString::<128>::new();
        for ch in header.chars().take(layout.text_cols) {
            let _ = header_trimmed.push(ch);
        }
        ctx.draw_text_at_char(area.x, header_y, header_trimmed.as_str(), ctx.colors.fg, header_bg);

        let highlight = apply_intensity(ctx.colors.fg, ctx.colors.bg, 48);
        let start = self.scroll.min(total_entries.saturating_sub(view_rows));
        for (row, entry) in self.entries.iter().skip(start).take(view_rows).enumerate() {
            let list_row = layout.list_row.saturating_add(row);
            let abs_row = area.y.saturating_add(list_row);
            let selected = start.saturating_add(row) == self.selection;
            let row_bg = if selected { highlight } else { ctx.colors.bg };
            if selected {
                ctx.fill_rect(
                    area.x.saturating_mul(char_w),
                    abs_row.saturating_mul(char_h),
                    area.w.saturating_mul(char_w),
                    char_h,
                    highlight,
                );
            }
            let mut name = HString::<128>::new();
            let mut label = entry.name.as_str();
            if matches!(entry.kind, FileEntryKind::Parent) {
                label = "..";
            }
            for ch in label.chars().take(name_cols.saturating_sub(1)) {
                let _ = name.push(ch);
            }
            if matches!(entry.kind, FileEntryKind::Dir | FileEntryKind::Parent) {
                let _ = name.push('\\');
            }
            ctx.draw_text_at_char(
                area.x,
                abs_row,
                name.as_str(),
                ctx.colors.fg,
                row_bg,
            );
            if let Some(size_col) = size_col {
                let size_label = if matches!(entry.kind, FileEntryKind::Dir | FileEntryKind::Parent) {
                    let mut s = HString::<16>::new();
                    let _ = s.push_str("<DIR>");
                    s
                } else {
                    self.format_size(entry.size)
                };
                let mut trimmed = HString::<16>::new();
                for ch in size_label.chars().take(FILE_EXPLORER_SIZE_COLS) {
                    let _ = trimmed.push(ch);
                }
                let col = area.x.saturating_add(size_col);
                ctx.draw_text_at_char(col, abs_row, trimmed.as_str(), ctx.colors.fg, row_bg);
            }
        }

        if let Some(scrollbar) = self.scrollbar_draw(ctx, &layout, total_entries) {
            ctx.draw_scrollbar(scrollbar.track, scrollbar.thumb);
        }

        if let Some(picker) = &self.app_picker {
            if let Some(picker_layout) = self.app_picker_layout(ctx, picker.apps.len()) {
                let panel_bg = apply_intensity(ctx.colors.bg, 0x000000, 30);
                let panel_col = area.x.saturating_add(picker_layout.x);
                let panel_row = area.y.saturating_add(picker_layout.y);
                let panel_x = panel_col.saturating_mul(char_w);
                let panel_y = panel_row.saturating_mul(char_h);
                let panel_w = picker_layout.w.saturating_mul(char_w);
                let panel_h = picker_layout.h.saturating_mul(char_h);
                ctx.fill_rect(panel_x, panel_y, panel_w, panel_h, panel_bg);

                let text_col = panel_col.saturating_add(picker_layout.text_col);
                let header_row = panel_row;
                ctx.draw_text_at_char(text_col, header_row, "Open with", ctx.colors.fg, panel_bg);

                let highlight = apply_intensity(ctx.colors.fg, panel_bg, 48);
                let start = picker.scroll.min(picker.apps.len().saturating_sub(picker_layout.list_rows));
                for (row, app_idx) in picker.apps.iter().skip(start).take(picker_layout.list_rows).enumerate() {
                    let abs_row = panel_row.saturating_add(picker_layout.list_row + row);
                    let selected = start.saturating_add(row) == picker.selection;
                    let row_bg = if selected { highlight } else { panel_bg };
                    if selected {
                        ctx.fill_rect(panel_x, abs_row.saturating_mul(char_h), panel_w, char_h, highlight);
                    }
                    let label = builtin_apps()
                        .get(*app_idx)
                        .map(|app| app.label)
                        .unwrap_or("App");
                    let mut trimmed = HString::<64>::new();
                    for ch in label.chars().take(picker_layout.text_cols) {
                        let _ = trimmed.push(ch);
                    }
                    ctx.draw_text_at_char(text_col, abs_row, trimmed.as_str(), ctx.colors.fg, row_bg);
                }

                if let Some(scrollbar) = self.app_picker_scrollbar(ctx, picker, &picker_layout) {
                    ctx.draw_scrollbar(scrollbar.track, scrollbar.thumb);
                }

                let border_color = apply_intensity(ctx.colors.fg, panel_bg, 80);
                let border = 1usize.min(panel_w).min(panel_h);
                if border > 0 {
                    ctx.fill_rect(panel_x, panel_y, panel_w, border, border_color);
                    ctx.fill_rect(panel_x, panel_y + panel_h.saturating_sub(border), panel_w, border, border_color);
                    ctx.fill_rect(panel_x, panel_y, border, panel_h, border_color);
                    ctx.fill_rect(panel_x + panel_w.saturating_sub(border), panel_y, border, panel_h, border_color);
                }
            }
        }
    }

    fn handle_key(&mut self, ctx: &mut AppContext, evt: &KeyEvent) -> AppEventResult {
        if let Some(picker) = &self.app_picker {
            let Some(layout) = self.app_picker_layout(ctx, picker.apps.len()) else {
                return AppEventResult::Ignored;
            };
            match evt {
                KeyEvent::Up => {
                    if self.app_picker_move(-1, &layout) {
                        return AppEventResult::HandledRedraw;
                    }
                    return AppEventResult::HandledNoRedraw;
                }
                KeyEvent::Down => {
                    if self.app_picker_move(1, &layout) {
                        return AppEventResult::HandledRedraw;
                    }
                    return AppEventResult::HandledNoRedraw;
                }
                KeyEvent::Enter => {
                    if self.app_picker_open() {
                        return AppEventResult::HandledRedraw;
                    }
                    return AppEventResult::HandledNoRedraw;
                }
                KeyEvent::Backspace => {
                    self.app_picker = None;
                    return AppEventResult::HandledRedraw;
                }
                _ => return AppEventResult::HandledNoRedraw,
            }
        }

        let total_entries = self.entries.len();
        let Some(layout) = self.layout(ctx, total_entries) else { return AppEventResult::Ignored; };
        let view_rows = layout.list_rows;
        match evt {
            KeyEvent::Up => {
                if self.move_selection(-1, view_rows) {
                    return AppEventResult::HandledRedraw;
                }
                AppEventResult::HandledNoRedraw
            }
            KeyEvent::Down => {
                if self.move_selection(1, view_rows) {
                    return AppEventResult::HandledRedraw;
                }
                AppEventResult::HandledNoRedraw
            }
            KeyEvent::Enter => {
                if self.open_selected(view_rows) {
                    return AppEventResult::HandledRedraw;
                }
                AppEventResult::HandledNoRedraw
            }
            KeyEvent::Backspace => {
                if !self.path.is_empty() {
                    self.path.pop();
                    self.refresh_entries();
                    self.ensure_selection_visible(view_rows);
                    return AppEventResult::HandledRedraw;
                }
                AppEventResult::HandledNoRedraw
            }
            _ => AppEventResult::Ignored,
        }
    }

    fn handle_mouse(&mut self, ctx: &mut AppContext, evt: &MouseEvent) -> AppEventResult {
        if matches!(evt.kind, MouseEventKind::Up) {
            return AppEventResult::Ignored;
        }
        if let Some(picker) = &self.app_picker {
            let Some(layout) = self.app_picker_layout(ctx, picker.apps.len()) else {
                return AppEventResult::Ignored;
            };
            let within = evt.col >= layout.x
                && evt.col < layout.x.saturating_add(layout.w)
                && evt.row >= layout.y
                && evt.row < layout.y.saturating_add(layout.h);
            if !within {
                self.app_picker = None;
                return AppEventResult::HandledRedraw;
            }
            if evt.row >= layout.y.saturating_add(layout.list_row)
                && evt.row < layout.y.saturating_add(layout.list_row + layout.list_rows)
            {
                let row = evt.row.saturating_sub(layout.y + layout.list_row);
                let idx = picker.scroll.saturating_add(row);
                if idx < picker.apps.len() {
                    if let Some(picker) = self.app_picker.as_mut() {
                        picker.selection = idx;
                    }
                    if evt.clicks >= 2 {
                        self.app_picker_open();
                        return AppEventResult::HandledRedraw;
                    }
                    return AppEventResult::HandledRedraw;
                }
            }
            return AppEventResult::HandledNoRedraw;
        }

        let total_entries = self.entries.len();
        let Some(layout) = self.layout(ctx, total_entries) else { return AppEventResult::Ignored; };
        if evt.row < FILE_EXPLORER_HEADER_ROWS {
            return AppEventResult::HandledNoRedraw;
        }
        let row = evt.row.saturating_sub(FILE_EXPLORER_HEADER_ROWS);
        if row >= layout.list_rows {
            return AppEventResult::HandledNoRedraw;
        }
        let idx = self.scroll.saturating_add(row);
        if idx >= self.entries.len() {
            return AppEventResult::HandledNoRedraw;
        }
        self.selection = idx;
        self.ensure_selection_visible(layout.list_rows);
        if evt.clicks >= 2 {
            self.open_selected(layout.list_rows);
        }
        AppEventResult::HandledRedraw
    }

    fn scroll_by(&mut self, ctx: &mut AppContext, lines: i32) -> bool {
        if lines == 0 {
            return false;
        }
        if let Some(app_count) = self.app_picker.as_ref().map(|picker| picker.apps.len()) {
            let Some(layout) = self.app_picker_layout(ctx, app_count) else { return false; };
            let Some(picker) = self.app_picker.as_mut() else { return false; };
            let max_scroll = picker.apps.len().saturating_sub(layout.list_rows) as i32;
            let mut next = picker.scroll as i32 + lines;
            if next < 0 {
                next = 0;
            }
            if next > max_scroll {
                next = max_scroll;
            }
            let next = next as usize;
            if next == picker.scroll {
                return false;
            }
            picker.scroll = next;
            return true;
        }
        let total_entries = self.entries.len();
        let Some(layout) = self.layout(ctx, total_entries) else { return false; };
        let max_scroll = self.max_scroll(layout.list_rows) as i32;
        let mut next = self.scroll as i32 + lines;
        if next < 0 {
            next = 0;
        }
        if next > max_scroll {
            next = max_scroll;
        }
        let next = next as usize;
        if next == self.scroll {
            return false;
        }
        self.scroll = next;
        true
    }

    fn scroll_to(&mut self, ctx: &mut AppContext, scroll: usize) -> bool {
        if let Some(app_count) = self.app_picker.as_ref().map(|picker| picker.apps.len()) {
            let Some(layout) = self.app_picker_layout(ctx, app_count) else { return false; };
            let Some(picker) = self.app_picker.as_mut() else { return false; };
            let max_scroll = picker.apps.len().saturating_sub(layout.list_rows);
            let scroll = scroll.min(max_scroll);
            if scroll == picker.scroll {
                return false;
            }
            picker.scroll = scroll;
            return true;
        }
        let total_entries = self.entries.len();
        let Some(layout) = self.layout(ctx, total_entries) else { return false; };
        let max_scroll = self.max_scroll(layout.list_rows);
        let scroll = scroll.min(max_scroll);
        if scroll == self.scroll {
            return false;
        }
        self.scroll = scroll;
        true
    }

    fn scroll_metrics(&self, ctx: &AppContext) -> Option<ScrollMetrics> {
        if let Some(picker) = &self.app_picker {
            let layout = self.app_picker_layout(ctx, picker.apps.len())?;
            if picker.apps.len() <= layout.list_rows {
                return None;
            }
            let char_w = ctx.metrics.char_w.max(1);
            let char_h = ctx.metrics.char_h.max(1);
            let scrollbar_col = layout.scrollbar_col?;
            let track = Rect {
                x: ctx.metrics.x.saturating_add(
                    (layout.x + scrollbar_col).saturating_mul(char_w),
                ),
                y: ctx.metrics.y.saturating_add(
                    (layout.y + layout.list_row).saturating_mul(char_h),
                ),
                w: char_w,
                h: layout.list_rows.saturating_mul(char_h),
            };
            let max_scroll = picker.apps.len().saturating_sub(layout.list_rows);
            let thumb_h = (track.h.saturating_mul(layout.list_rows) / picker.apps.len())
                .max(char_h)
                .min(track.h);
            let scroll = picker.scroll.min(max_scroll);
            return Some(ScrollMetrics { track, thumb_h, max_scroll, scroll });
        }

        let total_entries = self.entries.len();
        let layout = self.layout(ctx, total_entries)?;
        if total_entries <= layout.list_rows {
            return None;
        }
        let scrollbar_col = layout.scrollbar_col?;
        let char_w = ctx.metrics.char_w.max(1);
        let char_h = ctx.metrics.char_h.max(1);
        let track = Rect {
            x: ctx.metrics.x.saturating_add(
                (layout.area.x + scrollbar_col).saturating_mul(char_w),
            ),
            y: ctx.metrics.y.saturating_add(
                (layout.area.y + layout.list_row).saturating_mul(char_h),
            ),
            w: char_w,
            h: layout.list_rows.saturating_mul(char_h),
        };
        let max_scroll = total_entries.saturating_sub(layout.list_rows);
        let thumb_h = (track.h.saturating_mul(layout.list_rows) / total_entries)
            .max(char_h)
            .min(track.h);
        let scroll = self.scroll.min(max_scroll);
        Some(ScrollMetrics { track, thumb_h, max_scroll, scroll })
    }

    fn on_resize(&mut self, ctx: &AppContext) {
        let total_entries = self.entries.len();
        if let Some(layout) = self.layout(ctx, total_entries) {
            self.clamp_scroll(layout.list_rows);
        }
        if let Some(app_count) = self.app_picker.as_ref().map(|picker| picker.apps.len()) {
            if let Some(layout) = self.app_picker_layout(ctx, app_count) {
                let max_scroll = app_count.saturating_sub(layout.list_rows);
                if let Some(picker) = self.app_picker.as_mut() {
                    if picker.scroll > max_scroll {
                        picker.scroll = max_scroll;
                    }
                }
            }
        }
    }

    fn take_action(&mut self) -> Option<AppAction> {
        self.pending_action.take()
    }
}

struct TerminalLayout {
    area: ContentArea,
    output_rows: usize,
    text_cols: usize,
    scrollbar_col: Option<usize>,
}

struct TerminalApp;

impl TerminalApp {
    fn new() -> Self {
        TerminalApp
    }

    fn prompt_text(&self) -> HString<128> {
        let mut prompt = HString::<128>::new();
        if commands::is_prompt_path_enabled() {
            let path = fs::prompt_path();
            let _ = prompt.push_str(&path);
        }
        let _ = prompt.push_str(TERMINAL_PROMPT);
        prompt
    }

    fn input_visual_lines(&self, text_cols: usize, term: &terminal::TerminalState) -> usize {
        if text_cols == 0 {
            return 0;
        }
        let prompt_len = self.prompt_text().chars().count();
        let input_len = term.input.chars().count();
        let total_len = prompt_len.saturating_add(input_len);
        let lines = if total_len == 0 {
            1
        } else {
            (total_len + text_cols - 1) / text_cols
        };
        lines.max(1)
    }

    fn line_counts(&self, text_cols: usize) -> (usize, usize) {
        if text_cols == 0 {
            return (0, 0);
        }
        let output_lines = terminal::visual_line_count(text_cols);
        let input_lines = terminal::with_state(|term| self.input_visual_lines(text_cols, term));
        (output_lines, input_lines)
    }

    fn layout_for_area(&self, area: ContentArea, needs_scroll: bool) -> Option<TerminalLayout> {
        if area.h == 0 {
            return None;
        }
        let output_rows = area.h;
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

    fn layout_and_lines(&self, ctx: &AppContext) -> Option<(TerminalLayout, usize)> {
        let area = ctx.metrics.content_area?;
        let output_rows = area.h;
        let mut layout = self.layout_for_area(area, false)?;
        let mut text_cols = layout.text_cols.min(128);
        let (mut output_lines, mut input_lines) = self.line_counts(text_cols);
        let mut total_lines = output_lines.saturating_add(input_lines);
        let needs_scroll = output_rows > 0 && total_lines > output_rows;
        if needs_scroll {
            if let Some(scrolled) = self.layout_for_area(area, true) {
                layout = scrolled;
                text_cols = layout.text_cols.min(128);
                let counts = self.line_counts(text_cols);
                output_lines = counts.0;
                input_lines = counts.1;
                total_lines = output_lines.saturating_add(input_lines);
            }
        }
        terminal::set_view_with_extra(layout.output_rows, text_cols, input_lines);
        Some((layout, total_lines))
    }

    fn draw_output_range(
        &self,
        ctx: &AppContext,
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
            let fg = line.fg.unwrap_or(ctx.colors.fg);
            let bg = line.bg.unwrap_or(ctx.colors.bg);
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
                        ctx.draw_text_at_char(start_col, start_row + row, buf.as_str(), fg, bg);
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
                    ctx.draw_text_at_char(start_col, start_row + row, buf.as_str(), fg, bg);
                    row = row.saturating_add(1);
                    if row >= max_rows {
                        break 'lines;
                    }
                }
            }
        }
    }

    fn draw_input_range(
        &self,
        ctx: &AppContext,
        start_col: usize,
        start_row: usize,
        text_cols: usize,
        max_rows: usize,
        skip_rows: usize,
        input_focus: bool,
        term: &terminal::TerminalState,
    ) {
        if text_cols == 0 || max_rows == 0 {
            return;
        }
        let prompt = self.prompt_text();
        let prompt_len = prompt.chars().count();
        let input_len = term.input.chars().count();
        let total_len = prompt_len.saturating_add(input_len);
        if total_len == 0 {
            return;
        }
        let input_lines = (total_len + text_cols - 1) / text_cols;

        let mut row = 0usize;
        let mut col = 0usize;
        let mut buf = HString::<128>::new();
        for ch in prompt.chars().chain(term.input.chars()) {
            if col == text_cols {
                if row >= skip_rows && row < skip_rows.saturating_add(max_rows) {
                    ctx.draw_text_at_char(start_col, start_row + row - skip_rows, buf.as_str(), ctx.colors.fg, ctx.colors.bg);
                }
                buf.clear();
                row = row.saturating_add(1);
                col = 0;
            }
            let _ = buf.push(ch);
            col = col.saturating_add(1);
        }
        if row >= skip_rows && row < skip_rows.saturating_add(max_rows) {
            ctx.draw_text_at_char(start_col, start_row + row - skip_rows, buf.as_str(), ctx.colors.fg, ctx.colors.bg);
        }

        if input_focus {
            let suggestion = commands::suggest_command(term.input.as_str());
            let suggestion_suffix = suggestion
                .as_ref()
                .and_then(|s| s.as_str().get(term.input.as_str().len()..));
            if let Some(suffix) = suggestion_suffix {
                let ghost_color = apply_intensity(ctx.colors.fg, ctx.colors.bg, 100);
                let mut idx = prompt_len.saturating_add(input_len);
                for ch in suffix.chars() {
                    let row = idx / text_cols;
                    let col = idx % text_cols;
                    if row >= input_lines {
                        break;
                    }
                    if row >= skip_rows && row < skip_rows.saturating_add(max_rows) {
                        let mut ghost = HString::<2>::new();
                        let _ = ghost.push(ch);
                        ctx.draw_text_at_char(
                            start_col + col,
                            start_row + row - skip_rows,
                            ghost.as_str(),
                            ghost_color,
                            ctx.colors.bg,
                        );
                    }
                    idx = idx.saturating_add(1);
                }
            }
        }

        if input_focus {
            let cursor_pos = term.cursor_pos.min(input_len);
            let cursor_idx = prompt_len.saturating_add(cursor_pos);
            let cursor_row = cursor_idx / text_cols;
            let cursor_col = cursor_idx % text_cols;
            if cursor_row >= skip_rows && cursor_row < skip_rows.saturating_add(max_rows) {
                ctx.draw_text_cursor(start_col + cursor_col, start_row + cursor_row - skip_rows, ctx.colors.fg);
            }
        }
    }

    fn scrollbar_draw(&self, ctx: &AppContext, layout: &TerminalLayout, total_lines: usize) -> Option<ScrollbarDraw> {
        if layout.output_rows == 0 || total_lines <= layout.output_rows {
            return None;
        }
        let scrollbar_col = layout.scrollbar_col?;
        let char_w = ctx.metrics.char_w.max(1);
        let char_h = ctx.metrics.char_h.max(1);
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

    fn run_command(&self, line: &str) {
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
}

impl WindowApp for TerminalApp {
    fn accepts_input(&self) -> bool {
        true
    }

    fn draw(&mut self, ctx: &mut AppContext, input_focus: bool) {
        let Some((layout, total_lines)) = self.layout_and_lines(ctx) else { return; };
        let output_rows = layout.output_rows;
        let text_cols = layout.text_cols.min(128);
        if text_cols == 0 || output_rows == 0 {
            return;
        }
        let scroll = terminal::scroll();
        let start_row = layout.area.y;
        let start_col = layout.area.x;

        let output_lines = terminal::visual_line_count(text_cols);
        let input_lines = terminal::with_state(|term| self.input_visual_lines(text_cols, term));
        let output_visible = if scroll >= output_lines {
            0
        } else {
            output_lines.saturating_sub(scroll).min(output_rows)
        };
        let input_skip = scroll.saturating_sub(output_lines);
        let input_visible = output_rows.saturating_sub(output_visible);

        terminal::with_state(|term| {
            if output_visible > 0 {
                self.draw_output_range(
                    ctx,
                    start_col,
                    start_row,
                    text_cols,
                    output_visible,
                    scroll,
                    term,
                );
            }
            if input_visible > 0 && input_skip < input_lines {
                let input_row = start_row.saturating_add(output_visible);
                self.draw_input_range(
                    ctx,
                    start_col,
                    input_row,
                    text_cols,
                    input_visible,
                    input_skip,
                    input_focus,
                    term,
                );
            }
        });

        if let Some(scrollbar) = self.scrollbar_draw(ctx, &layout, total_lines) {
            ctx.draw_scrollbar(scrollbar.track, scrollbar.thumb);
        }
    }

    fn handle_key(&mut self, ctx: &mut AppContext, evt: &KeyEvent) -> AppEventResult {
        let prompt = self.prompt_text();
        let mut handled = false;
        let mut changed = false;
        let mut entered: Option<HString<128>> = None;

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
            let trimmed = line.as_str().trim();
            if !trimmed.is_empty() {
                let mut echoed = HString::<256>::new();
                let _ = echoed.push_str(prompt.as_str());
                let _ = echoed.push_str(line.as_str());
                terminal::push_output(echoed.as_str(), true);
                self.run_command(line.as_str());
                history::push(line.as_str());
            } else {
                terminal::push_output(prompt.as_str(), true);
            }
            changed = true;
        }

        if handled {
            if changed {
                let _ = self.layout_and_lines(ctx);
                if terminal::scroll() < terminal::max_scroll() {
                    terminal::set_scroll(usize::MAX);
                    let _ = self.layout_and_lines(ctx);
                }
                return AppEventResult::HandledRedraw;
            }
            return AppEventResult::HandledNoRedraw;
        }
        AppEventResult::Ignored
    }

    fn handle_mouse(&mut self, ctx: &mut AppContext, evt: &MouseEvent) -> AppEventResult {
        if matches!(evt.kind, MouseEventKind::Up) {
            return AppEventResult::Ignored;
        }
        let Some((layout, _)) = self.layout_and_lines(ctx) else { return AppEventResult::Ignored; };
        let text_cols = layout.text_cols.min(128);
        if text_cols == 0 || evt.col >= text_cols || evt.row >= layout.output_rows {
            return AppEventResult::HandledNoRedraw;
        }
        let scroll = terminal::scroll();
        let output_lines = terminal::visual_line_count(text_cols);
        let input_lines = terminal::with_state(|term| self.input_visual_lines(text_cols, term));
        let combined_row = scroll.saturating_add(evt.row);
        if combined_row < output_lines || combined_row >= output_lines.saturating_add(input_lines) {
            return AppEventResult::HandledNoRedraw;
        }
        let input_row = combined_row.saturating_sub(output_lines);
        let prompt = self.prompt_text();
        let prompt_len = prompt.chars().count();
        terminal::with_state_mut(|term| {
            let len = term.input.chars().count();
            term.selection_anchor = None;
            let idx = input_row.saturating_mul(text_cols).saturating_add(evt.col);
            if idx <= prompt_len {
                term.cursor_pos = 0;
            } else {
                let pos = idx.saturating_sub(prompt_len);
                term.cursor_pos = pos.min(len);
            }
        });
        AppEventResult::HandledRedraw
    }

    fn scroll_by(&mut self, ctx: &mut AppContext, lines: i32) -> bool {
        if lines == 0 {
            return false;
        }
        let Some((layout, _)) = self.layout_and_lines(ctx) else { return false; };
        if layout.output_rows == 0 {
            return false;
        }
        terminal::scroll_by(lines)
    }

    fn scroll_to(&mut self, ctx: &mut AppContext, scroll: usize) -> bool {
        let Some((layout, _)) = self.layout_and_lines(ctx) else { return false; };
        if layout.output_rows == 0 {
            return false;
        }
        terminal::set_scroll(scroll)
    }

    fn scroll_metrics(&self, ctx: &AppContext) -> Option<ScrollMetrics> {
        let (layout, total_lines) = self.layout_and_lines(ctx)?;
        if layout.output_rows == 0 || total_lines <= layout.output_rows {
            return None;
        }
        let scrollbar_col = layout.scrollbar_col?;
        let char_w = ctx.metrics.char_w.max(1);
        let char_h = ctx.metrics.char_h.max(1);
        let track = Rect {
            x: ctx.metrics.x.saturating_add(scrollbar_col.saturating_mul(char_w)),
            y: ctx.metrics.y.saturating_add(layout.area.y.saturating_mul(char_h)),
            w: char_w,
            h: layout.output_rows.saturating_mul(char_h),
        };
        let max_scroll = total_lines.saturating_sub(layout.output_rows);
        let thumb_h = (track.h.saturating_mul(layout.output_rows) / total_lines)
            .max(char_h)
            .min(track.h);
        let scroll = terminal::scroll().min(max_scroll);
        Some(ScrollMetrics { track, thumb_h, max_scroll, scroll })
    }
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
