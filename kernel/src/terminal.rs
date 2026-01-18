use alloc::vec::Vec;
use heapless::String as HString;
use spin::Mutex;
use x86_64::instructions::interrupts;

const TERMINAL_MAX_LINES: usize = 256;
const TERMINAL_MAX_COLS: usize = 128;

pub struct TerminalLine {
    pub text: HString<128>,
    pub fg: Option<u32>,
    pub bg: Option<u32>,
}

impl TerminalLine {
    fn new() -> Self {
        Self { text: HString::new(), fg: None, bg: None }
    }

    fn with_color(fg: Option<u32>, bg: Option<u32>) -> Self {
        Self { text: HString::new(), fg, bg }
    }
}

pub struct TerminalState {
    pub lines: Vec<TerminalLine>,
    pub input: HString<128>,
    pub cursor_pos: usize,
    pub selection_anchor: Option<usize>,
    pub history_index: Option<usize>,
    pub draft_line: HString<128>,
    pub scroll: usize,
    view_rows: usize,
    view_cols: usize,
    pinned: bool,
}

impl TerminalState {
    const fn new() -> Self {
        Self {
            lines: Vec::new(),
            input: HString::new(),
            cursor_pos: 0,
            selection_anchor: None,
            history_index: None,
            draft_line: HString::new(),
            scroll: 0,
            view_rows: 0,
            view_cols: 0,
            pinned: true,
        }
    }

    fn ensure_line(&mut self) {
        if self.lines.is_empty() {
            self.lines.push(TerminalLine::new());
        }
    }

    fn new_line(&mut self) {
        self.new_line_with_color(None, None);
    }

    fn new_line_with_color(&mut self, fg: Option<u32>, bg: Option<u32>) {
        if self.lines.len() >= TERMINAL_MAX_LINES {
            self.lines.remove(0);
            if self.scroll > 0 {
                self.scroll = self.scroll.saturating_sub(1);
            }
        }
        self.lines.push(TerminalLine::with_color(fg, bg));
    }

    fn append_to_last(&mut self, text: &str, fg: Option<u32>, bg: Option<u32>) {
        self.ensure_line();
        let Some(last) = self.lines.last_mut() else { return; };
        if last.text.is_empty() {
            if fg.is_some() {
                last.fg = fg;
            }
            if bg.is_some() {
                last.bg = bg;
            }
        }
        for ch in text.chars() {
            if last.text.len() >= TERMINAL_MAX_COLS {
                break;
            }
            let _ = last.text.push(ch);
        }
    }

    fn push_output_with_color(&mut self, text: &str, newline: bool, fg: Option<u32>, bg: Option<u32>) {
        let was_pinned = self.pinned || self.scroll >= self.max_scroll();
        self.ensure_line();
        if (fg.is_some() || bg.is_some())
            && self.lines.last().map(|line| !line.text.is_empty()).unwrap_or(false)
        {
            self.new_line_with_color(fg, bg);
        }
        let mut parts = text.split('\n');
        if let Some(first) = parts.next() {
            self.append_to_last(first, fg, bg);
        }
        for part in parts {
            self.new_line_with_color(fg, bg);
            self.append_to_last(part, fg, bg);
        }
        if newline {
            self.new_line();
        }
        if was_pinned {
            self.scroll = self.max_scroll();
        }
        self.pinned = self.scroll >= self.max_scroll();
    }

    fn push_output(&mut self, text: &str, newline: bool) {
        self.push_output_with_color(text, newline, None, None);
    }

    fn visual_line_count_for(&self, cols: usize) -> usize {
        if cols == 0 {
            return 0;
        }
        let mut total = 0usize;
        for line in &self.lines {
            let len = line.text.chars().count();
            let segments = if len == 0 {
                1
            } else {
                (len + cols - 1) / cols
            };
            total = total.saturating_add(segments);
        }
        total
    }

    fn visual_line_count(&self) -> usize {
        self.visual_line_count_for(self.view_cols)
    }

    fn max_scroll(&self) -> usize {
        if self.view_rows == 0 || self.view_cols == 0 {
            return 0;
        }
        self.visual_line_count().saturating_sub(self.view_rows)
    }

    fn clamp_scroll(&mut self) {
        let max_scroll = self.max_scroll();
        if self.scroll > max_scroll {
            self.scroll = max_scroll;
        }
    }

    fn set_view(&mut self, rows: usize, cols: usize) {
        self.view_rows = rows;
        self.view_cols = cols;
        self.clamp_scroll();
        if self.pinned {
            self.scroll = self.max_scroll();
        }
        self.pinned = self.scroll >= self.max_scroll();
    }

    fn scroll_by(&mut self, delta: i32) -> bool {
        if self.view_rows == 0 {
            return false;
        }
        let max_scroll = self.max_scroll() as i32;
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
        self.pinned = self.scroll >= self.max_scroll();
        true
    }

    fn set_scroll(&mut self, scroll: usize) -> bool {
        let max_scroll = self.max_scroll();
        let scroll = scroll.min(max_scroll);
        if scroll == self.scroll {
            return false;
        }
        self.scroll = scroll;
        self.pinned = self.scroll >= self.max_scroll();
        true
    }
}

static TERMINAL: Mutex<TerminalState> = Mutex::new(TerminalState::new());

pub fn with_state_mut<F, R>(f: F) -> R
where
    F: FnOnce(&mut TerminalState) -> R,
{
    interrupts::without_interrupts(|| {
        let mut state = TERMINAL.lock();
        f(&mut state)
    })
}

pub fn with_state<F, R>(f: F) -> R
where
    F: FnOnce(&TerminalState) -> R,
{
    interrupts::without_interrupts(|| {
        let state = TERMINAL.lock();
        f(&state)
    })
}

pub fn console_output_hook(text: &str, newline: bool) {
    with_state_mut(|term| term.push_output(text, newline));
}

pub fn push_output(text: &str, newline: bool) {
    with_state_mut(|term| term.push_output(text, newline));
}

pub fn push_output_colored(text: &str, fg: u32, newline: bool) {
    with_state_mut(|term| term.push_output_with_color(text, newline, Some(fg), None));
}

pub fn clear_output() {
    with_state_mut(|term| {
        term.lines.clear();
        term.lines.push(TerminalLine::new());
        term.scroll = 0;
        term.pinned = true;
    });
}

#[allow(dead_code)]
pub fn line_count() -> usize {
    with_state(|term| term.visual_line_count())
}

pub fn visual_line_count(cols: usize) -> usize {
    with_state(|term| term.visual_line_count_for(cols))
}

pub fn set_view(rows: usize, cols: usize) {
    with_state_mut(|term| term.set_view(rows, cols));
}

#[allow(dead_code)]
pub fn set_view_rows(rows: usize) {
    with_state_mut(|term| term.set_view(rows, term.view_cols));
}

pub fn scroll_by(delta: i32) -> bool {
    with_state_mut(|term| term.scroll_by(delta))
}

pub fn set_scroll(scroll: usize) -> bool {
    with_state_mut(|term| term.set_scroll(scroll))
}

pub fn scroll() -> usize {
    with_state(|term| term.scroll)
}

#[allow(dead_code)]
pub fn input_clear() {
    with_state_mut(|term| {
        term.input.clear();
        term.cursor_pos = 0;
        term.selection_anchor = None;
    });
}
