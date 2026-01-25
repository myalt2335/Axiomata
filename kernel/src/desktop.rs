use alloc::boxed::Box;
use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, Ordering};
use heapless::String as HString;

use crate::commands;
use crate::console::{self, CompositorMode, CursorBlink, CursorStyle, LayerId};
use crate::desktop_apps;
use crate::keyboard::{KeyEvent, Keyboard};
use crate::mouse;
use crate::run_mode::{self, RunMode};
use crate::serial;
use crate::terminal;
use crate::windows::{
    draw_scrollbar, AppAction, AppColors, AppContext, AppDescriptor, ContentArea, MouseEvent, MouseEventKind, Rect,
    ScrollMetrics, WindowApp, WindowMetrics,
};
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
const SCROLL_LINES_PER_NOTCH: i32 = 3;
const CURSOR_HOT_X: usize = 2;
const CURSOR_HOT_Y: usize = 2;
const START_MENU_MIN_COLS: usize = 30;
const START_MENU_MAX_COLS: usize = 56;
const START_MENU_MIN_ROWS: usize = 12;
const START_MENU_MAX_ROWS: usize = 18;
const START_MENU_HEADER_ROWS: usize = 2;
const START_MENU_FOOTER_ROWS: usize = 2;
const START_MENU_PAD_COLS: usize = 2;
const START_MENU_POWER_LABEL: &str = "Pwr";
const DOUBLE_CLICK_MAX_DIST: usize = 4;
const RESIZE_TICK_INTERVAL: u64 = 4;
const RESIZE_OUTLINE_COLOR: u32 = 0xFFFFFF;
const RESIZE_OUTLINE_MIN_THICKNESS: usize = 2;
const RESIZE_OUTLINE_MAX_THICKNESS: usize = 4;
static RESIZE_OUTLINE_ENABLED: AtomicBool = AtomicBool::new(false);
static MOVE_OUTLINE_ENABLED: AtomicBool = AtomicBool::new(false);

pub fn resize_outline_enabled() -> bool {
    RESIZE_OUTLINE_ENABLED.load(Ordering::Acquire)
}


pub fn toggle_resize_outline_enabled() -> bool {
    let prev = RESIZE_OUTLINE_ENABLED.fetch_xor(true, Ordering::AcqRel);
    !prev
}

pub fn move_outline_enabled() -> bool {
    MOVE_OUTLINE_ENABLED.load(Ordering::Acquire)
}


pub fn toggle_move_outline_enabled() -> bool {
    let prev = MOVE_OUTLINE_ENABLED.fetch_xor(true, Ordering::AcqRel);
    !prev
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum FocusInput {
    Auto,
    Clear,
}

struct TaskbarItem {
    window: usize,
    rect: Rect,
}

struct ScrollbarInfo {
    target: ScrollTarget,
    track: Rect,
    thumb: Rect,
}

#[derive(Copy, Clone)]
enum StartMenuAction {
    App(usize),
    PowerToggle,
    Restart,
    Shutdown,
}

struct StartMenuLayout {
    rect: Rect,
    list_rect: Rect,
    list_rows: usize,
    list_text_cols: usize,
    needs_scroll: bool,
    power_button: Option<Rect>,
    power_menu_rect: Option<Rect>,
}

struct Window {
    id: LayerId,
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    title: HString<32>,
    z: i16,
    minimized: bool,
    app: Box<dyn WindowApp>,
}

#[derive(Copy, Clone)]
struct ConsoleSnapshot {
    fg: u32,
    bg: u32,
    cursor_style: CursorStyle,
    cursor_blink: CursorBlink,
    cursor_color: u32,
    compositor_mode: CompositorMode,
    classic_mode: bool,
    hud_rows: usize,
}

impl ConsoleSnapshot {
    fn capture() -> Self {
        let (fg, bg) = console::default_colors();
        Self {
            fg,
            bg,
            cursor_style: console::cursor_style(),
            cursor_blink: console::cursor_blink(),
            cursor_color: console::cursor_color(),
            compositor_mode: console::compositor_mode(),
            classic_mode: console::is_classic_mode(),
            hud_rows: console::reserved_hud_rows(),
        }
    }

    fn restore(self) {
        console::set_compositor_mode(self.compositor_mode);
        console::set_classic_mode(self.classic_mode);
        console::set_default_colors(self.fg, self.bg);
        console::reserve_hud_rows(self.hud_rows);
        console::set_cursor_style(self.cursor_style);
        console::set_cursor_blink(self.cursor_blink);
        console::set_cursor_color(self.cursor_color);
    }
}

struct Desktop {
    windows: Vec<Window>,
    apps: &'static [AppDescriptor],
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
    resize_pending: Option<PendingResize>,
    last_resize_tick: u64,
    resize_outline_window: Option<usize>,
    scroll_drag: Option<ScrollDrag>,
    input_focus: Option<usize>,
    taskbar_items: Vec<TaskbarItem>,
    start_menu_items: Vec<usize>,
    start_button: Option<Rect>,
    start_menu: Option<LayerId>,
    start_menu_rect: Option<Rect>,
    start_open: bool,
    start_menu_scroll: usize,
    start_power_open: bool,
    last_click_tick: u64,
    last_click_window: Option<usize>,
    last_click_x: usize,
    last_click_y: usize,
    console_snapshot: ConsoleSnapshot,
}

struct DragState {
    window: usize,
    grab_x: i32,
    grab_y: i32,
    mode: DragMode,
}

#[derive(Copy, Clone)]
struct PendingResize {
    window: usize,
    w: usize,
    h: usize,
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
    Window(usize),
    StartMenu,
}

#[derive(Copy, Clone)]
struct ScrollDrag {
    target: ScrollTarget,
    grab_offset: i32,
}

pub fn run() -> RunMode {
    if !console::has_scene_buffer() {
        console::write_line("Desktop mode requires a scene buffer; falling back to console.");
        run_mode::request(RunMode::Console);
        return RunMode::Console;
    }
    let mut desktop = Desktop::new();
    let mut keyboard = Keyboard::new();
    let mut last_bar_tick = timer::ticks();

    loop {
        if let Some(next) = run_mode::should_switch() {
            desktop.shutdown();
            return next;
        }
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

    fn double_click_ticks(&self) -> u64 {
        let freq = timer::frequency() as u64;
        (freq / 3).max(1)
    }

    fn resize_outline_thickness(&self) -> usize {
        let base = self.char_w.max(self.char_h) / 3;
        let mut thickness = base.max(RESIZE_OUTLINE_MIN_THICKNESS);
        if thickness > RESIZE_OUTLINE_MAX_THICKNESS {
            thickness = RESIZE_OUTLINE_MAX_THICKNESS;
        }
        thickness
    }

    fn mouse_event_for_window(
        &self,
        win: &Window,
        mx: usize,
        my: usize,
        kind: MouseEventKind,
        clicks: u8,
    ) -> Option<MouseEvent> {
        let area = self.content_area(win)?;
        let rect = self.content_rect_px(win)?;
        if !rect.contains(mx, my) {
            return None;
        }
        let char_w = self.char_w.max(1);
        let char_h = self.char_h.max(1);
        if char_w == 0 || char_h == 0 {
            return None;
        }
        let col = (mx.saturating_sub(win.x) / char_w).saturating_sub(area.x);
        let row = (my.saturating_sub(win.y) / char_h).saturating_sub(area.y);
        if col >= area.w || row >= area.h {
            return None;
        }
        Some(MouseEvent { col, row, kind, clicks })
    }

    fn app_colors(&self) -> AppColors {
        AppColors {
            fg: WINDOW_FG,
            bg: WINDOW_BG,
            scrollbar_bg: SCROLLBAR_BG,
            scrollbar_thumb: SCROLLBAR_THUMB,
        }
    }

    fn window_metrics(&self, idx: usize) -> Option<WindowMetrics> {
        let win = self.windows.get(idx)?;
        Some(self.window_metrics_for(win))
    }

    fn window_metrics_for(&self, win: &Window) -> WindowMetrics {
        WindowMetrics {
            id: win.id,
            x: win.x,
            y: win.y,
            w: win.w,
            h: win.h,
            char_w: self.char_w,
            char_h: self.char_h,
            content_area: self.content_area(win),
        }
    }

    fn new() -> Self {
        let console_snapshot = ConsoleSnapshot::capture();
        let stats = console::display_buffer_stats().expect("Display unavailable");
        let (cols, rows) = console::size_chars();
        let char_w = (stats.width_px / cols.max(1)).max(1);
        let char_h = (stats.height_px / rows.max(1)).max(1);

        console::reserve_hud_rows(0);
        console::set_classic_mode(false);
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

        let apps = desktop_apps::builtin_apps();
        let mut start_menu_items = Vec::new();
        for (idx, app) in apps.iter().enumerate() {
            if app.start_menu {
                start_menu_items.push(idx);
            }
        }

        let mut desktop = Self {
            windows: Vec::new(),
            apps,
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
            resize_pending: None,
            last_resize_tick: 0,
            resize_outline_window: None,
            scroll_drag: None,
            input_focus: None,
            taskbar_items: Vec::new(),
            start_menu_items,
            start_button: None,
            start_menu: None,
            start_menu_rect: None,
            start_open: false,
            start_menu_scroll: 0,
            start_power_open: false,
            last_click_tick: 0,
            last_click_window: None,
            last_click_x: 0,
            last_click_y: 0,
            console_snapshot,
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

    fn shutdown(&mut self) {
        for win in self.windows.drain(..) {
            console::destroy_layer(win.id);
        }
        if let Some(id) = self.start_menu.take() {
            console::destroy_layer(id);
        }
        if let Some(id) = self.taskbar.take() {
            console::destroy_layer(id);
        }
        if let Some(id) = self.cursor.take() {
            console::destroy_layer(id);
        }
        console::clear_resize_outline();
        self.resize_outline_window = None;
        self.console_snapshot.restore();
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
        if idx >= self.windows.len() {
            self.input_focus = None;
            return false;
        }
        if self.windows.get(idx).map(|win| win.minimized).unwrap_or(true) {
            self.input_focus = None;
            return false;
        }

        let Some(metrics) = self.window_metrics(idx) else { return false; };
        let mut ctx = AppContext {
            metrics,
            colors: self.app_colors(),
        };
        let result = {
            let win = &mut self.windows[idx];
            win.app.handle_key(&mut ctx, evt)
        };

        let action = {
            let win = &mut self.windows[idx];
            win.app.take_action()
        };

        let mut updated = false;
        if result.needs_redraw() {
            let active = self.focused == Some(idx);
            self.draw_window(idx, active);
            updated = true;
        }
        if let Some(action) = action {
            updated = self.handle_app_action(action) || updated;
        }

        if !result.handled() {
            return updated;
        }
        true
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

        let outline_allowed = if let Some(drag) = &self.drag {
            match drag.mode {
                DragMode::Move => move_outline_enabled(),
                DragMode::Resize { .. } => resize_outline_enabled(),
            }
        } else {
            false
        };
        if !outline_allowed {
            if let Some(idx) = self.resize_outline_window.take() {
                console::clear_resize_outline();
                if let Some(win) = self.windows.get(idx) {
                    console::layer_set_pos(win.id, win.x, win.y);
                    console::layer_set_visible(win.id, true);
                }
                updated = true;
            }
        }

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
                let mut lines = wheel.saturating_mul(SCROLL_LINES_PER_NOTCH);
                if !commands::is_scroll_inverted() {
                    lines = -lines;
                }
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

            if self.start_open && self.start_power_open {
                if let Some(layout) = self.start_menu_layout() {
                    let mut keep_open = false;
                    if let Some(button) = layout.power_button {
                        keep_open = keep_open || button.contains(mx, my);
                    }
                    if let Some(menu) = layout.power_menu_rect {
                        keep_open = keep_open || menu.contains(mx, my);
                    }
                    if !keep_open {
                        self.start_power_open = false;
                        self.draw_start_menu();
                    }
                }
            }

            if let Some(action) = self.start_menu_action_at(mx, my) {
                let close_menu = self.handle_start_action(action);
                if close_menu {
                    self.set_start_menu_open(false);
                    taskbar_dirty = true;
                }
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
                        self.resize_pending = Some(PendingResize {
                            window: idx,
                            w: win.w,
                            h: win.h,
                        });
                        self.last_resize_tick = timer::ticks();
                        if resize_outline_enabled() {
                            console::layer_set_visible(win.id, false);
                            self.resize_outline_window = Some(idx);
                            let thickness = self.resize_outline_thickness();
                            console::set_resize_outline(win.x, win.y, win.w, win.h, thickness, RESIZE_OUTLINE_COLOR);
                        }
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
                        if move_outline_enabled() {
                            console::layer_set_visible(win.id, false);
                            self.resize_outline_window = Some(idx);
                            let thickness = self.resize_outline_thickness();
                            console::set_resize_outline(win.x, win.y, win.w, win.h, thickness, RESIZE_OUTLINE_COLOR);
                        }
                    }
                } else if let Some(win) = self.windows.get(idx) {
                    let now = timer::ticks();
                    let mut clicks = 1u8;
                    if self.last_click_window == Some(idx)
                        && now.wrapping_sub(self.last_click_tick) <= self.double_click_ticks()
                    {
                        let dx = mx.saturating_sub(self.last_click_x).max(self.last_click_x.saturating_sub(mx));
                        let dy = my.saturating_sub(self.last_click_y).max(self.last_click_y.saturating_sub(my));
                        if dx <= DOUBLE_CLICK_MAX_DIST && dy <= DOUBLE_CLICK_MAX_DIST {
                            clicks = 2;
                        }
                    }
                    self.last_click_tick = now;
                    self.last_click_window = Some(idx);
                    self.last_click_x = mx;
                    self.last_click_y = my;

                    if let Some(evt) = self.mouse_event_for_window(win, mx, my, MouseEventKind::Down, clicks) {
                        if let Some(metrics) = self.window_metrics(idx) {
                            let mut ctx = AppContext {
                                metrics,
                                colors: self.app_colors(),
                            };
                            let result = {
                                let win = &mut self.windows[idx];
                                win.app.handle_mouse(&mut ctx, &evt)
                            };
                            let action = {
                                let win = &mut self.windows[idx];
                                win.app.take_action()
                            };
                            if result.needs_redraw() {
                                let active = self.focused == Some(idx);
                                self.draw_window(idx, active);
                                updated = true;
                            }
                            if let Some(action) = action {
                                updated = self.handle_app_action(action) || updated;
                                taskbar_dirty = true;
                            }
                            if result.handled() {
                                if taskbar_dirty {
                                    self.draw_taskbar();
                                }
                                return true;
                            }
                        }
                    }
                }
            } else {
                self.input_focus = None;
                self.last_click_window = None;
            }
        }

        let was_dragging = self.drag.is_some();
        let mut finalize_resize = false;
        if let Some(drag) = &self.drag {
            if left_down {
                let mode = drag.mode;
                match mode {
                    DragMode::Move => {
                        let thickness = if move_outline_enabled() {
                            Some(self.resize_outline_thickness())
                        } else {
                            None
                        };
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
                                if move_outline_enabled() {
                                    if self.resize_outline_window != Some(drag.window) {
                                        console::layer_set_visible(win.id, false);
                                        self.resize_outline_window = Some(drag.window);
                                    }
                                    if let Some(thickness) = thickness {
                                        console::set_resize_outline(new_x, new_y, win.w, win.h, thickness, RESIZE_OUTLINE_COLOR);
                                    }
                                } else {
                                    console::layer_set_pos(win.id, new_x, new_y);
                                }
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
                            let mut new_w = clamp_dim(new_w, self.min_window_w(), max_w);
                            let mut new_h = clamp_dim(new_h, self.min_window_h(), max_h);
                            new_w = snap_dim(new_w, self.min_window_w(), max_w, self.char_w.max(1));
                            new_h = snap_dim(new_h, self.min_window_h(), max_h, self.char_h.max(1));
                            self.resize_pending = Some(PendingResize {
                                window: drag.window,
                                w: new_w,
                                h: new_h,
                            });
                            if resize_outline_enabled() {
                                if self.resize_outline_window != Some(drag.window) {
                                    console::layer_set_visible(win.id, false);
                                    self.resize_outline_window = Some(drag.window);
                                }
                                let thickness = self.resize_outline_thickness();
                                console::set_resize_outline(win.x, win.y, new_w, new_h, thickness, RESIZE_OUTLINE_COLOR);
                                updated = true;
                            } else {
                                let now = timer::ticks();
                                if now.wrapping_sub(self.last_resize_tick) >= RESIZE_TICK_INTERVAL {
                                    if new_w != win.w || new_h != win.h {
                                        self.resize_window_to(drag.window, new_w, new_h);
                                        updated = true;
                                    }
                                    self.last_resize_tick = now;
                                }
                            }
                        } else {
                            self.drag = None;
                        }
                    }
                }
            } else if left_released {
                finalize_resize = matches!(drag.mode, DragMode::Resize { .. });
                self.drag = None;
            }
        } else if left_released {
            self.drag = None;
        }

        if finalize_resize {
            let pending = self.resize_pending.take();
            if resize_outline_enabled() {
                console::clear_resize_outline();
                self.resize_outline_window = None;
            }
            if let Some(pending) = pending {
                if pending.window < self.windows.len() {
                    let win = &self.windows[pending.window];
                    if pending.w != win.w || pending.h != win.h {
                        self.resize_window_to(pending.window, pending.w, pending.h);
                        updated = true;
                    } else if resize_outline_enabled() {
                        console::layer_set_visible(win.id, true);
                        updated = true;
                    }
                }
            }
        } else if left_released && was_dragging {
            if self.resize_outline_window.is_some() {
                console::clear_resize_outline();
                if let Some(idx) = self.resize_outline_window.take() {
                    if let Some(win) = self.windows.get(idx) {
                        console::layer_set_pos(win.id, win.x, win.y);
                        console::layer_set_visible(win.id, true);
                    }
                }
                updated = true;
            }
            self.resize_pending = None;
        }

        if left_released && !was_dragging {
            if !self.is_in_taskbar(my) && !self.start_menu_contains(mx, my) {
                if let Some(idx) = self.window_at(mx, my) {
                    if let Some(win) = self.windows.get(idx) {
                        if let Some(evt) = self.mouse_event_for_window(win, mx, my, MouseEventKind::Up, 1) {
                            if let Some(metrics) = self.window_metrics(idx) {
                                let mut ctx = AppContext {
                                    metrics,
                                    colors: self.app_colors(),
                                };
                                let result = {
                                    let win = &mut self.windows[idx];
                                    win.app.handle_mouse(&mut ctx, &evt)
                                };
                                let action = {
                                    let win = &mut self.windows[idx];
                                    win.app.take_action()
                                };
                                if result.needs_redraw() {
                                    let active = self.focused == Some(idx);
                                    self.draw_window(idx, active);
                                    updated = true;
                                }
                                if let Some(action) = action {
                                    updated = self.handle_app_action(action) || updated;
                                    taskbar_dirty = true;
                                }
                                if result.handled() {
                                    if taskbar_dirty {
                                        self.draw_taskbar();
                                    }
                                    return true;
                                }
                            }
                        }
                    }
                }
            }
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
        for (idx, app) in self.apps.iter().enumerate() {
            if !app.startup {
                continue;
            }
            let title = self.next_app_title(app.default_title);
            let _ = self.create_window(pad, pad, w1.max(1), h1.max(1), title.as_str(), idx);
        }
    }

    fn spawn_app_window(&mut self, app_idx: usize) {
        let Some(app) = self.apps.get(app_idx) else { return; };
        let title = self.next_app_title(app.default_title);
        let count = self.windows.len() as i32;
        let offset = (count * 12).rem_euclid(80) as usize;
        let w = self.min_window_w().max(self.screen_w / 3);
        let h = self.min_window_h().max(self.work_h / 3);
        let x = (24 + offset).min(self.screen_w.saturating_sub(w));
        let y = (24 + offset).min(self.work_h.saturating_sub(h));
        let _ = self.create_window(x, y, w, h, title.as_str(), app_idx);
    }

    fn spawn_app_window_with_path(&mut self, app_idx: usize, path: &str) {
        let Some(app) = self.apps.get(app_idx) else { return; };
        let title = self.next_app_title(app.default_title);
        let count = self.windows.len() as i32;
        let offset = (count * 12).rem_euclid(80) as usize;
        let w = self.min_window_w().max(self.screen_w / 3);
        let h = self.min_window_h().max(self.work_h / 3);
        let x = (24 + offset).min(self.screen_w.saturating_sub(w));
        let y = (24 + offset).min(self.work_h.saturating_sub(h));
        let Some(idx) = self.create_window(x, y, w, h, title.as_str(), app_idx) else { return; };
        if let Some(win) = self.windows.get_mut(idx) {
            if win.app.open_path(path) {
                let active = self.focused == Some(idx);
                self.draw_window(idx, active);
            }
        }
    }

    fn handle_app_action(&mut self, action: AppAction) -> bool {
        match action {
            AppAction::OpenFile { app_idx, path } => {
                self.spawn_app_window_with_path(app_idx, &path);
                self.draw_taskbar();
                true
            }
        }
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

    fn create_window(&mut self, x: usize, y: usize, w: usize, h: usize, title: &str, app_id: usize) -> Option<usize> {
        let w = w.min(self.screen_w.max(1)).max(1);
        let h = h.min(self.work_h.max(1)).max(1);
        let x = x.min(self.screen_w.saturating_sub(w));
        let y = y.min(self.work_h.saturating_sub(h));

        let app = self.apps.get(app_id).map(|desc| (desc.factory)())?;
        let z = self.next_z;
        self.next_z = self.next_z.saturating_add(WINDOW_Z_STEP);
        let id = console::create_layer(w, h, x, y, z, 255)?;

        let mut title_buf = HString::<32>::new();
        let _ = title_buf.push_str(title);
        let win = Window { id, x, y, w, h, title: title_buf, z, minimized: false, app };
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
            .map(|win| win.app.accepts_input() && !win.minimized)
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

    fn draw_window(&mut self, idx: usize, active: bool) {
        let (id, w, h, title, minimized) = match self.windows.get(idx) {
            Some(win) => (win.id, win.w, win.h, win.title.clone(), win.minimized),
            None => return,
        };
        if minimized {
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

        console::layer_clear(id, WINDOW_BG);

        let title_h = self.title_bar_height().min(h);
        console::layer_fill_rect(id, 0, 0, w, title_h, title_color);

        let border = BORDER_THICKNESS.min(w).min(h);
        if border > 0 {
            console::layer_fill_rect(id, 0, 0, w, border, border_color);
            if h > border {
                console::layer_fill_rect(id, 0, h - border, w, border, border_color);
            }
            console::layer_fill_rect(id, 0, 0, border, h, border_color);
            if w > border {
                console::layer_fill_rect(id, w - border, 0, border, h, border_color);
            }
        }

        if self.char_w > 0 && self.char_h > 0 {
            let input_focus = self.input_focus == Some(idx);
            console::layer_draw_text_at_char(id, 1, 0, title.as_str(), WINDOW_FG, title_color);
            if let Some(win) = self.windows.get(idx) {
                self.draw_window_controls(win, title_color);
            }
            let Some(metrics) = self.window_metrics(idx) else { return; };
            let mut ctx = AppContext {
                metrics,
                colors: self.app_colors(),
            };
            if let Some(win) = self.windows.get_mut(idx) {
                win.app.draw(&mut ctx, input_focus);
            }
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
        if self.start_open {
            if let Some(layout) = self.start_menu_layout() {
                if layout.list_rect.contains(x, y) {
                    return Some(ScrollTarget::StartMenu);
                }
            }
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
        let ctx = AppContext {
            metrics: self.window_metrics_for(win),
            colors: self.app_colors(),
        };
        if win.app.scroll_metrics(&ctx).is_some() {
            Some(ScrollTarget::Window(idx))
        } else {
            None
        }
    }

    fn scroll_target_for_window(&self, idx: usize) -> Option<ScrollTarget> {
        let win = self.windows.get(idx)?;
        if win.minimized {
            return None;
        }
        let ctx = AppContext {
            metrics: self.window_metrics_for(win),
            colors: self.app_colors(),
        };
        if win.app.scroll_metrics(&ctx).is_some() {
            Some(ScrollTarget::Window(idx))
        } else {
            None
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
                let ctx = AppContext {
                    metrics: self.window_metrics_for(win),
                    colors: self.app_colors(),
                };
                if win.app.scroll_metrics(&ctx).is_some() {
                    if let Some(info) = self.scrollbar_info_for_target(ScrollTarget::Window(idx)) {
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
            ScrollTarget::Window(idx) => self.set_window_scroll(idx, new_scroll),
            ScrollTarget::StartMenu => self.set_start_menu_scroll(new_scroll),
        }
    }

    fn scroll_metrics(&self, target: ScrollTarget) -> Option<ScrollMetrics> {
        match target {
            ScrollTarget::Window(idx) => {
                let win = self.windows.get(idx)?;
                if win.minimized {
                    return None;
                }
                let ctx = AppContext {
                    metrics: self.window_metrics_for(win),
                    colors: self.app_colors(),
                };
                win.app.scroll_metrics(&ctx)
            }
            ScrollTarget::StartMenu => self.start_menu_scroll_metrics(),
        }
    }

    fn scrollbar_info_for_target(&self, target: ScrollTarget) -> Option<ScrollbarInfo> {
        let metrics = self.scroll_metrics(target)?;
        let scroll = metrics.scroll.min(metrics.max_scroll);
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

    fn start_menu_scroll_metrics(&self) -> Option<ScrollMetrics> {
        if !self.start_open {
            return None;
        }
        let layout = self.start_menu_layout()?;
        let total = self.start_menu_items.len();
        let visible = layout.list_rows;
        if visible == 0 || total <= visible || !layout.needs_scroll {
            return None;
        }
        let char_w = self.char_w.max(1);
        let char_h = self.char_h.max(1);
        if char_w == 0 || char_h == 0 {
            return None;
        }
        let track = Rect {
            x: layout.list_rect.x.saturating_add(layout.list_rect.w.saturating_sub(char_w)),
            y: layout.list_rect.y,
            w: char_w,
            h: layout.list_rect.h,
        };
        let max_scroll = total.saturating_sub(visible);
        let thumb_h = (track.h.saturating_mul(visible) / total)
            .max(char_h)
            .min(track.h);
        Some(ScrollMetrics {
            track,
            thumb_h,
            max_scroll,
            scroll: self.start_menu_scroll.min(max_scroll),
        })
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
            ScrollTarget::Window(idx) => self.scroll_window(idx, lines),
            ScrollTarget::StartMenu => self.scroll_start_menu(lines),
        }
    }

    fn set_window_scroll(&mut self, idx: usize, scroll: usize) -> bool {
        let Some(metrics) = self.window_metrics(idx) else { return false; };
        let mut ctx = AppContext {
            metrics,
            colors: self.app_colors(),
        };
        let Some(win) = self.windows.get_mut(idx) else { return false; };
        if win.minimized {
            return false;
        }
        let changed = win.app.scroll_to(&mut ctx, scroll);
        if !changed {
            return false;
        }
        let active = self.focused == Some(idx);
        self.draw_window(idx, active);
        true
    }

    fn scroll_window(&mut self, idx: usize, lines: i32) -> bool {
        let Some(metrics) = self.window_metrics(idx) else { return false; };
        let mut ctx = AppContext {
            metrics,
            colors: self.app_colors(),
        };
        let Some(win) = self.windows.get_mut(idx) else { return false; };
        if win.minimized {
            return false;
        }
        let changed = win.app.scroll_by(&mut ctx, lines);
        if !changed {
            return false;
        }
        let active = self.focused == Some(idx);
        self.draw_window(idx, active);
        true
    }

    fn start_menu_layout(&self) -> Option<StartMenuLayout> {
        if self.start_menu_items.is_empty() {
            return None;
        }
        let char_w = self.char_w.max(1);
        let char_h = self.char_h.max(1);
        if char_w == 0 || char_h == 0 {
            return None;
        }
        let max_cols = self.screen_w / char_w;
        let max_rows = self.taskbar_y / char_h;
        if max_cols == 0 || max_rows == 0 {
            return None;
        }

        let min_rows = START_MENU_MIN_ROWS.min(max_rows);
        let menu_rows = max_rows.min(START_MENU_MAX_ROWS).max(min_rows);
        let mut header_rows = START_MENU_HEADER_ROWS.min(menu_rows);
        let mut footer_rows = START_MENU_FOOTER_ROWS.min(menu_rows.saturating_sub(header_rows));
        if menu_rows < header_rows + footer_rows + 1 {
            footer_rows = footer_rows.min(menu_rows.saturating_sub(1));
            if menu_rows < header_rows + footer_rows + 1 {
                header_rows = header_rows.min(menu_rows.saturating_sub(footer_rows + 1));
            }
        }
        let list_rows = menu_rows.saturating_sub(header_rows + footer_rows).max(1);

        let needs_scroll = self.start_menu_items.len() > list_rows;
        let scroll_cols = if needs_scroll { 1 } else { 0 };
        let max_len = self
            .start_menu_items
            .iter()
            .filter_map(|idx| self.apps.get(*idx).map(|app| app.label.chars().count()))
            .max()
            .unwrap_or(0);
        let list_needed_cols = max_len.saturating_add(1).saturating_add(scroll_cols);
        let power_menu_len = ["Restart", "Shutdown"]
            .iter()
            .map(|label| label.chars().count())
            .max()
            .unwrap_or(0);
        let power_menu_cols = power_menu_len.saturating_add(2);
        let content_needed_cols = list_needed_cols.max(power_menu_cols);
        let mut menu_cols = content_needed_cols.saturating_add(START_MENU_PAD_COLS.saturating_mul(2));
        menu_cols = menu_cols.max(START_MENU_MIN_COLS);
        menu_cols = menu_cols.min(START_MENU_MAX_COLS);
        menu_cols = menu_cols.min(max_cols.max(1));

        let pad_cols = START_MENU_PAD_COLS.min(menu_cols.saturating_sub(2) / 2);
        let list_col = pad_cols;
        let list_cols = menu_cols.saturating_sub(pad_cols.saturating_mul(2)).max(1);
        let list_text_cols = list_cols.saturating_sub(scroll_cols);

        let menu_w = menu_cols.saturating_mul(char_w);
        let menu_h = menu_rows.saturating_mul(char_h);
        let start_x = self.start_button.map(|rect| rect.x).unwrap_or(0);
        let x = start_x.min(self.screen_w.saturating_sub(menu_w));
        let y = self.taskbar_y.saturating_sub(menu_h);
        let rect = Rect { x, y, w: menu_w, h: menu_h };

        let list_row = header_rows;
        let list_rect = Rect {
            x: rect.x.saturating_add(list_col.saturating_mul(char_w)),
            y: rect.y.saturating_add(list_row.saturating_mul(char_h)),
            w: list_cols.saturating_mul(char_w),
            h: list_rows.saturating_mul(char_h),
        };

        let mut power_button = None;
        let mut power_menu_rect = None;
        if footer_rows >= 1 {
            let power_row = menu_rows.saturating_sub(footer_rows);
            let power_label_len = START_MENU_POWER_LABEL.chars().count();
            let power_cols = power_label_len.saturating_add(2).min(list_cols).max(1);
            let power_col = list_col.saturating_add(list_cols.saturating_sub(power_cols));
            let button_x = rect.x.saturating_add(power_col.saturating_mul(char_w));
            let button_y = rect.y.saturating_add(power_row.saturating_mul(char_h));
            power_button = Some(Rect {
                x: button_x,
                y: button_y,
                w: power_cols.saturating_mul(char_w),
                h: char_h,
            });

            let menu_rows = 2usize;
            if menu_rows > 0 {
                let menu_cols = power_menu_cols.min(list_cols).max(1);
                let menu_w = menu_cols.saturating_mul(char_w);
                let menu_h = menu_rows.saturating_mul(char_h);
                let min_x = rect.x.saturating_add(list_col.saturating_mul(char_w));
                let max_x = rect.x.saturating_add(rect.w.saturating_sub(menu_w));
                let mut menu_x = button_x.saturating_add(power_cols.saturating_mul(char_w)).saturating_sub(menu_w);
                if menu_x < min_x {
                    menu_x = min_x;
                }
                if menu_x > max_x {
                    menu_x = max_x;
                }
                let min_y = rect.y.saturating_add(header_rows.saturating_mul(char_h));
                let mut menu_y = button_y.saturating_sub(menu_h);
                if menu_y < min_y {
                    menu_y = min_y;
                }
                power_menu_rect = Some(Rect { x: menu_x, y: menu_y, w: menu_w, h: menu_h });
            }
        }

        Some(StartMenuLayout {
            rect,
            list_rect,
            list_rows,
            list_text_cols,
            needs_scroll,
            power_button,
            power_menu_rect,
        })
    }

    fn start_menu_visible_rows(&self) -> usize {
        self.start_menu_layout().map(|layout| layout.list_rows).unwrap_or(0)
    }

    fn scroll_start_menu(&mut self, lines: i32) -> bool {
        if !self.start_open {
            return false;
        }
        let visible = self.start_menu_visible_rows();
        if visible == 0 {
            return false;
        }
        let max_scroll = self.start_menu_items.len().saturating_sub(visible) as i32;
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
        let max_scroll = self.start_menu_items.len().saturating_sub(visible);
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
            self.start_power_open = false;
            self.draw_start_menu();
        } else {
            if matches!(self.scroll_drag.as_ref().map(|drag| drag.target), Some(ScrollTarget::StartMenu)) {
                self.scroll_drag = None;
            }
            self.start_power_open = false;
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

    fn start_menu_action_at(&self, x: usize, y: usize) -> Option<StartMenuAction> {
        if !self.start_open {
            return None;
        }
        let layout = self.start_menu_layout()?;
        if !layout.rect.contains(x, y) {
            return None;
        }
        if let Some(button) = layout.power_button {
            if button.contains(x, y) {
                return Some(StartMenuAction::PowerToggle);
            }
        }
        if self.start_power_open {
            if let Some(menu) = layout.power_menu_rect {
                if menu.contains(x, y) {
                    let char_h = self.char_h.max(1);
                    if char_h == 0 {
                        return None;
                    }
                    let row = (y - menu.y) / char_h;
                    return match row {
                        0 => Some(StartMenuAction::Restart),
                        1 => Some(StartMenuAction::Shutdown),
                        _ => None,
                    };
                }
            }
        }
        if !layout.list_rect.contains(x, y) {
            return None;
        }
        let char_h = self.char_h.max(1);
        if char_h == 0 {
            return None;
        }
        let char_w = self.char_w.max(1);
        if layout.needs_scroll && layout.list_text_cols == 0 {
            return None;
        }
        if layout.needs_scroll && layout.list_text_cols > 0 {
            let scrollbar_x = layout.list_rect.x.saturating_add(layout.list_text_cols.saturating_mul(char_w));
            if x >= scrollbar_x {
                return None;
            }
        }
        let row = (y - layout.list_rect.y) / char_h;
        let idx = self.start_menu_scroll.saturating_add(row);
        self.start_menu_items
            .get(idx)
            .copied()
            .map(StartMenuAction::App)
    }

    fn handle_start_action(&mut self, action: StartMenuAction) -> bool {
        match action {
            StartMenuAction::App(app_idx) => {
                self.spawn_app_window(app_idx);
                true
            }
            StartMenuAction::PowerToggle => {
                self.start_power_open = !self.start_power_open;
                self.draw_start_menu();
                false
            }
            StartMenuAction::Restart => {
                console::set_output_hook(Some(terminal::console_output_hook));
                commands::reboot();
                console::set_output_hook(None);
                true
            }
            StartMenuAction::Shutdown => {
                console::set_output_hook(Some(terminal::console_output_hook));
                commands::shutdown();
            }
        }
    }

    fn draw_start_menu(&mut self) {
        let Some(layout) = self.start_menu_layout() else { return; };
        if self.start_power_open && layout.power_button.is_none() {
            self.start_power_open = false;
        }
        let rect = layout.rect;
        let char_w = self.char_w.max(1);
        let char_h = self.char_h.max(1);
        if char_w == 0 || char_h == 0 {
            return;
        }
        let menu_cols = rect.w / char_w;
        let menu_rows = rect.h / char_h;
        if menu_cols == 0 || menu_rows == 0 {
            return;
        }
        let max_scroll = self.start_menu_items.len().saturating_sub(layout.list_rows);
        if self.start_menu_scroll > max_scroll {
            self.start_menu_scroll = max_scroll;
        }

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

        let list_row = (layout.list_rect.y.saturating_sub(rect.y)) / char_h;
        let list_col = (layout.list_rect.x.saturating_sub(rect.x)) / char_w;
        let header_rows = list_row;
        let footer_row = list_row.saturating_add(layout.list_rows);
        if header_rows > 0 {
            let header_h = header_rows.saturating_mul(char_h);
            console::layer_fill_rect(id, 0, 0, rect.w, header_h, TASKBAR_ACCENT);
        }
        if footer_row < menu_rows {
            let footer_y = footer_row.saturating_mul(char_h);
            let footer_h = rect.h.saturating_sub(footer_y);
            console::layer_fill_rect(id, 0, footer_y, rect.w, footer_h, TASKBAR_ACCENT);
        }
        if header_rows > 0 {
            let sep_y = header_rows.saturating_mul(char_h);
            if sep_y < rect.h {
                console::layer_fill_rect(id, 0, sep_y, rect.w, 1, START_MENU_BORDER);
            }
        }
        if footer_row < menu_rows {
            let sep_y = footer_row.saturating_mul(char_h);
            if sep_y < rect.h {
                console::layer_fill_rect(id, 0, sep_y, rect.w, 1, START_MENU_BORDER);
            }
        }
        if header_rows > 0 {
            let header_text_row = header_rows.saturating_sub(1);
            console::layer_draw_text_at_char(id, list_col, header_text_row, "Start", TASKBAR_FG, TASKBAR_ACCENT);
        }
        for (i, app_idx) in self
            .start_menu_items
            .iter()
            .skip(self.start_menu_scroll)
            .take(layout.list_rows)
            .enumerate()
        {
            if let Some(app) = self.apps.get(*app_idx) {
                let mut label = HString::<64>::new();
                for ch in app.label.chars().take(layout.list_text_cols) {
                    let _ = label.push(ch);
                }
                console::layer_draw_text_at_char(id, list_col, list_row + i, label.as_str(), TASKBAR_FG, START_MENU_BG);
            }
        }

        if let Some(button) = layout.power_button {
            let row = (button.y.saturating_sub(rect.y)) / char_h;
            let bg = if self.start_power_open {
                TASKBAR_ITEM_ACTIVE_BG
            } else {
                TASKBAR_ITEM_BG
            };
            console::layer_fill_rect(
                id,
                button.x.saturating_sub(rect.x),
                button.y.saturating_sub(rect.y),
                button.w,
                button.h,
                bg,
            );
            let button_cols = button.w / char_w;
            let mut trimmed = HString::<16>::new();
            for ch in START_MENU_POWER_LABEL.chars().take(button_cols) {
                let _ = trimmed.push(ch);
            }
            let label_len = trimmed.chars().count();
            let col = button
                .x
                .saturating_sub(rect.x)
                .saturating_div(char_w)
                .saturating_add(button_cols.saturating_sub(label_len) / 2);
            console::layer_draw_text_at_char(id, col, row, trimmed.as_str(), TASKBAR_FG, bg);
        }

        if self.start_power_open {
            if let Some(menu) = layout.power_menu_rect {
                let menu_x = menu.x.saturating_sub(rect.x);
                let menu_y = menu.y.saturating_sub(rect.y);
                console::layer_fill_rect(id, menu_x, menu_y, menu.w, menu.h, TASKBAR_ITEM_BG);
                let menu_cols = menu.w / char_w;
                let text_cols = menu_cols.saturating_sub(2).max(1);
                let start_col = menu_x / char_w + 1;
                let start_row = menu_y / char_h;
                let items = ["Restart", "Shutdown"];
                for (idx, label) in items.iter().enumerate() {
                    let mut trimmed = HString::<16>::new();
                    for ch in label.chars().take(text_cols) {
                        let _ = trimmed.push(ch);
                    }
                    console::layer_draw_text_at_char(
                        id,
                        start_col,
                        start_row.saturating_add(idx),
                        trimmed.as_str(),
                        TASKBAR_FG,
                        TASKBAR_ITEM_BG,
                    );
                }
                let border = 1usize.min(menu.w).min(menu.h);
                if border > 0 {
                    console::layer_fill_rect(id, menu_x, menu_y, menu.w, border, START_MENU_BORDER);
                    console::layer_fill_rect(
                        id,
                        menu_x,
                        menu_y.saturating_add(menu.h.saturating_sub(border)),
                        menu.w,
                        border,
                        START_MENU_BORDER,
                    );
                    console::layer_fill_rect(id, menu_x, menu_y, border, menu.h, START_MENU_BORDER);
                    console::layer_fill_rect(
                        id,
                        menu_x.saturating_add(menu.w.saturating_sub(border)),
                        menu_y,
                        border,
                        menu.h,
                        START_MENU_BORDER,
                    );
                }
            }
        }

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
            draw_scrollbar(id, track, thumb, SCROLLBAR_BG, SCROLLBAR_THUMB);
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
                ScrollTarget::Window(win_idx) => {
                    if win_idx == idx {
                        self.scroll_drag = None;
                    } else if win_idx > idx {
                        drag.target = ScrollTarget::Window(win_idx - 1);
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
            if matches!(self.scroll_drag.as_ref().map(|drag| drag.target), Some(ScrollTarget::Window(win_idx)) if win_idx == idx) {
                self.scroll_drag = None;
            }
        } else {
            let active = self.focused == Some(idx);
            self.draw_window(idx, active);
        }
        true
    }

    fn resize_window_to(&mut self, idx: usize, new_w: usize, new_h: usize) {
        let Some((old_w, old_h)) = self.windows.get(idx).map(|win| (win.w, win.h)) else {
            self.drag = None;
            return;
        };
        if new_w == old_w && new_h == old_h {
            return;
        }
        self.rebuild_window(idx, new_w, new_h);
        if idx >= self.windows.len() {
            self.drag = None;
        }
    }

    fn rebuild_window(&mut self, idx: usize, new_w: usize, new_h: usize) {
        let Some((old_id, old_w, old_h, x, y, z)) = self
            .windows
            .get(idx)
            .map(|win| (win.id, win.w, win.h, win.x, win.y, win.z))
        else {
            return;
        };
        console::destroy_layer(old_id);
        let new_id = console::create_layer(new_w, new_h, x, y, z, 255);
        if let Some(new_id) = new_id {
            if let Some(win) = self.windows.get_mut(idx) {
                win.id = new_id;
                win.w = new_w;
                win.h = new_h;
            }
            let ctx = {
                let win = &self.windows[idx];
                AppContext {
                    metrics: self.window_metrics_for(win),
                    colors: self.app_colors(),
                }
            };
            if let Some(win) = self.windows.get_mut(idx) {
                win.app.on_resize(&ctx);
            }
            let active = self.focused == Some(idx);
            self.draw_window(idx, active);
            return;
        }

        let fallback_id = console::create_layer(old_w, old_h, x, y, z, 255);
        if let Some(fallback_id) = fallback_id {
            if let Some(win) = self.windows.get_mut(idx) {
                win.id = fallback_id;
            }
            let ctx = {
                let win = &self.windows[idx];
                AppContext {
                    metrics: self.window_metrics_for(win),
                    colors: self.app_colors(),
                }
            };
            if let Some(win) = self.windows.get_mut(idx) {
                win.app.on_resize(&ctx);
            }
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

fn snap_dim(value: usize, min: usize, max: usize, step: usize) -> usize {
    let mut v = if step <= 1 {
        value
    } else {
        (value / step) * step
    };
    if v < min {
        v = min;
    }
    if v > max {
        v = max;
    }
    v
}
