#![no_std]
#![no_main]
#![feature(abi_x86_interrupt)]

extern crate alloc;
extern crate spin;
extern crate lazy_static;

mod clipboard;
mod cdmo;
mod debug;
mod console;
mod block;
mod ata;
mod pci;
mod fat32;
mod ext2;
mod keyboard;
mod font;
mod font2;
mod font3;
mod boot_splash;
mod commands;
mod fs;
mod forth;
mod editor;
mod desktop_apps;
mod desktop;
mod windows;
mod run_mode;
mod terminal;
mod mouse;
mod ps2;
mod theme_presets;
mod help;
mod history;
mod memory;
mod timer;
mod interrupts;
mod pic;
mod serial;
mod time;
mod thud;
mod wait;
mod thudmodules {
    pub mod tin;
    pub mod min;
    pub mod utin;
}

use bootloader_api::{config::BootloaderConfig, entry_point, BootInfo};
use alloc::format;
use core::panic::PanicInfo;
use console::{init_console, with_console};
use keyboard::Keyboard;
use crate::run_mode::RunMode;
use heapless::{String, Vec};
use x86_64::instructions::interrupts as cpu_intr;
#[cfg(target_arch = "x86_64")]
use core::arch::asm;

pub const OS_NAME: &str = "Axiomata";
pub const OS_VERSION: &str = "4.A033.09.250124.EXENUS@cfc8a";
pub const OS_CN: &str = "Praxis";
const PROMPT: &str = ">";
const SHOWSPLASH: bool = false; // This kinda sucks bad. It's a cool-ish gimmick but no thank you for the actual OS.

const BOOT_MODE: RunMode = RunMode::Desktop;

static BOOTLOADER_CONFIG: BootloaderConfig = {
    let mut cfg = BootloaderConfig::new_default();
    cfg.mappings.physical_memory =
        Some(bootloader_api::config::Mapping::FixedAddress(0xffff_8000_0000_0000));
    cfg
};

entry_point!(kernel_main, config = &BOOTLOADER_CONFIG);

#[allow(dead_code)]
fn prompt_text() -> alloc::string::String {
    if commands::is_prompt_path_enabled() && !forth::is_active() && !editor::is_active() {
        format!("{}{}", fs::prompt_path(), PROMPT)
    } else {
        format!("{}", PROMPT)
    }
}

fn kernel_main(boot_info: &'static mut BootInfo) -> ! {
    enable_sse();
    serial::write("Hello from kernel!");
    memory::init_memory(boot_info);
    ata::init();

    init_console(boot_info);
    let ps2_ok = ps2::init_controller();
    let kbd_ok = keyboard::init();
    let mouse_ok = mouse::init();
    if !ps2_ok {
        serial::write("ps2: controller init failed");
    }
    if !kbd_ok {
        serial::write("ps2: keyboard init failed");
    }
    if !mouse_ok {
        serial::write("ps2: mouse init failed");
    }
    let hud_rows = if matches!(BOOT_MODE, RunMode::Console) { 1 } else { 0 };
    with_console(|c| c.reserve_hud_rows(hud_rows));
    thud::init();
    thudmodules::utin::init();
    thudmodules::min::init();
    thudmodules::tin::init();

    interrupts::init_idt();
    pic::init_pic();
    timer::init_pit();
    cpu_intr::enable();
    time::init_time();
    wait::init();
    fs::init_persistent();

    if SHOWSPLASH {
    boot_splash::show();
    }

    run_mode::init(BOOT_MODE);
    let mut mode = BOOT_MODE;
    loop {
        run_mode::enter(mode);
        match mode {
            RunMode::Console => {
                mode = run_console_shell();
            }
            RunMode::Desktop => {
                if !console::has_scene_buffer() {
                    console::write_line("Desktop mode requires a scene buffer; falling back to console.");
                    run_mode::request(RunMode::Console);
                    mode = RunMode::Console;
                    continue;
                }
                mode = desktop::run();
            }
        }
    }
}

fn enable_sse() {
    use x86_64::registers::control::{Cr0, Cr0Flags, Cr4, Cr4Flags};
    unsafe {
        Cr0::update(|flags| {
            flags.remove(Cr0Flags::EMULATE_COPROCESSOR);
            flags.remove(Cr0Flags::TASK_SWITCHED);
            flags.insert(Cr0Flags::MONITOR_COPROCESSOR);
        });
        Cr4::update(|flags| {
            flags.insert(Cr4Flags::OSFXSR);
            flags.insert(Cr4Flags::OSXMMEXCPT_ENABLE);
        });
        let mxcsr: u32 = 0x1F80;
        asm!("ldmxcsr [{}]", in(reg) &mxcsr, options(nostack, preserves_flags));
    }
}

#[allow(dead_code)]
fn run_console_shell() -> RunMode {
    let mut input_origin = with_console(|c| {
        c.clear();
        c.newline();
        c.write_line(&format!("{} {}\n", OS_NAME, OS_CN));
        c.write_line("A product of Stratocompute Technologies\n");
        c.write_line(OS_VERSION);
        c.newline();
        let prompt = prompt_text();
        c.write(&prompt);
        c.cursor_position()
    });

    let mut kbd = Keyboard::new();
    let mut line = String::<128>::new();
    let mut draft_line = String::<128>::new();
    let mut history_index: Option<usize> = None;
    let mut cursor_pos: usize = 0;
    let mut rendered_len: usize = 0;
    let mut selection_anchor: Option<usize> = None;

    loop {
        if let Some(next) = run_mode::should_switch() {
            return next;
        }
        if let Some(evt) = kbd.poll_event() {
            match evt {
                keyboard::KeyEvent::Char(ch) => {
                    if let Some(anchor) = selection_anchor {
                         delete_selection(&mut line, &mut cursor_pos, anchor);
                         selection_anchor = None;
                    }
                    if insert_char_at(&mut line, cursor_pos, ch) {
                        cursor_pos += 1;
                        redraw_input_line(&line, cursor_pos, input_origin, &mut rendered_len, selection_anchor);
                    }
                    history_index = None;
                }
                keyboard::KeyEvent::CtrlBackspace => {
                    selection_anchor = None;
                    if delete_prev_word(&mut line, &mut cursor_pos) {
                        redraw_input_line(&line, cursor_pos, input_origin, &mut rendered_len, selection_anchor);
                    }
                    history_index = None;
                }
                keyboard::KeyEvent::Backspace => {
                    if let Some(anchor) = selection_anchor {
                        delete_selection(&mut line, &mut cursor_pos, anchor);
                        selection_anchor = None;
                        redraw_input_line(&line, cursor_pos, input_origin, &mut rendered_len, selection_anchor);
                    } else if cursor_pos > 0 && remove_char_at(&mut line, cursor_pos - 1) {
                        cursor_pos -= 1;
                        redraw_input_line(&line, cursor_pos, input_origin, &mut rendered_len, selection_anchor);
                    }
                    history_index = None;
                }
                keyboard::KeyEvent::Delete => {
                    if let Some(anchor) = selection_anchor {
                        delete_selection(&mut line, &mut cursor_pos, anchor);
                        selection_anchor = None;
                        redraw_input_line(&line, cursor_pos, input_origin, &mut rendered_len, selection_anchor);
                    } else if remove_char_at(&mut line, cursor_pos) {
                        redraw_input_line(&line, cursor_pos, input_origin, &mut rendered_len, selection_anchor);
                    }
                    history_index = None;
                }
                keyboard::KeyEvent::Left => {
                    selection_anchor = None;
                    if cursor_pos > 0 {
                        cursor_pos -= 1;
                        redraw_input_line(&line, cursor_pos, input_origin, &mut rendered_len, selection_anchor);
                    } else {
                        redraw_input_line(&line, cursor_pos, input_origin, &mut rendered_len, selection_anchor);
                    }
                }
                keyboard::KeyEvent::Right => {
                    selection_anchor = None;
                    let len = line.chars().count();
                    if cursor_pos < len {
                        cursor_pos += 1;
                        redraw_input_line(&line, cursor_pos, input_origin, &mut rendered_len, selection_anchor);
                    } else {
                        redraw_input_line(&line, cursor_pos, input_origin, &mut rendered_len, selection_anchor);
                    }
                }
                keyboard::KeyEvent::CtrlLeft => {
                    selection_anchor = None;
                    if move_cursor_word_left(&line, &mut cursor_pos) {
                        redraw_input_line(&line, cursor_pos, input_origin, &mut rendered_len, selection_anchor);
                    } else {
                         redraw_input_line(&line, cursor_pos, input_origin, &mut rendered_len, selection_anchor);
                    }
                }
                keyboard::KeyEvent::CtrlRight => {
                    selection_anchor = None;
                    if move_cursor_word_right(&line, &mut cursor_pos) {
                        redraw_input_line(&line, cursor_pos, input_origin, &mut rendered_len, selection_anchor);
                    } else {
                         redraw_input_line(&line, cursor_pos, input_origin, &mut rendered_len, selection_anchor);
                    }
                }
                keyboard::KeyEvent::ShiftLeft => {
                    if selection_anchor.is_none() {
                        selection_anchor = Some(cursor_pos);
                    }
                    if cursor_pos > 0 {
                        cursor_pos -= 1;
                    }
                    redraw_input_line(&line, cursor_pos, input_origin, &mut rendered_len, selection_anchor);
                }
                keyboard::KeyEvent::ShiftRight => {
                    if selection_anchor.is_none() {
                        selection_anchor = Some(cursor_pos);
                    }
                    let len = line.chars().count();
                    if cursor_pos < len {
                        cursor_pos += 1;
                    }
                    redraw_input_line(&line, cursor_pos, input_origin, &mut rendered_len, selection_anchor);
                }
                keyboard::KeyEvent::CtrlShiftLeft => {
                    if selection_anchor.is_none() {
                        selection_anchor = Some(cursor_pos);
                    }
                    move_cursor_word_left(&line, &mut cursor_pos);
                    redraw_input_line(&line, cursor_pos, input_origin, &mut rendered_len, selection_anchor);
                }
                keyboard::KeyEvent::CtrlShiftRight => {
                    if selection_anchor.is_none() {
                        selection_anchor = Some(cursor_pos);
                    }
                    move_cursor_word_right(&line, &mut cursor_pos);
                    redraw_input_line(&line, cursor_pos, input_origin, &mut rendered_len, selection_anchor);
                }
                keyboard::KeyEvent::CtrlA => {
                    selection_anchor = Some(0);
                    cursor_pos = line.chars().count();
                    redraw_input_line(&line, cursor_pos, input_origin, &mut rendered_len, selection_anchor);
                }
                keyboard::KeyEvent::CtrlC => {
                    if let Some(anchor) = selection_anchor {
                        let start = anchor.min(cursor_pos);
                        let end = anchor.max(cursor_pos);
                        let selected_text: String<128> = line.chars().skip(start).take(end - start).collect();
                        clipboard::set_text(&selected_text);
                    }
                }
                keyboard::KeyEvent::CtrlX => {
                    if let Some(anchor) = selection_anchor {
                        let start = anchor.min(cursor_pos);
                        let end = anchor.max(cursor_pos);
                        let selected_text: String<128> = line.chars().skip(start).take(end - start).collect();
                        clipboard::set_text(&selected_text);
                        delete_selection(&mut line, &mut cursor_pos, anchor);
                        selection_anchor = None;
                        redraw_input_line(&line, cursor_pos, input_origin, &mut rendered_len, selection_anchor);
                    }
                }
                keyboard::KeyEvent::CtrlV => {
                    if let Some(anchor) = selection_anchor {
                        delete_selection(&mut line, &mut cursor_pos, anchor);
                        selection_anchor = None;
                    }
                    
                    let clip_text = clipboard::get_text();
                    for ch in clip_text.chars() {
                         if insert_char_at(&mut line, cursor_pos, ch) {
                            cursor_pos += 1;
                         } else {
                             break;
                         }
                    }
                    redraw_input_line(&line, cursor_pos, input_origin, &mut rendered_len, selection_anchor);
                }
                keyboard::KeyEvent::CtrlO | keyboard::KeyEvent::CtrlS => {}
                keyboard::KeyEvent::Up => {
                    selection_anchor = None;
                    let hist_len = history::len();
                    if hist_len == 0 {
                        continue;
                    }
                    if history_index.is_none() {
                        draft_line.clear();
                        let _ = draft_line.push_str(&line);
                    }
                    let new_idx = history_index
                        .map(|i| i.saturating_sub(1))
                        .unwrap_or_else(|| hist_len.saturating_sub(1));
                    if let Some(new_line) = history::entry(new_idx) {
                        history_index = Some(new_idx);
                        set_input_line(&mut line, &new_line, &mut cursor_pos, input_origin, &mut rendered_len);
                    } else {
                        history_index = None;
                    }
                }
                keyboard::KeyEvent::Down => {
                    selection_anchor = None;
                    let hist_len = history::len();
                    if hist_len == 0 {
                        continue;
                    }
                    if let Some(idx) = history_index {
                        if idx + 1 < hist_len {
                            let new_idx = idx + 1;
                            if let Some(new_line) = history::entry(new_idx) {
                                history_index = Some(new_idx);
                                set_input_line(&mut line, &new_line, &mut cursor_pos, input_origin, &mut rendered_len);
                            } else {
                                history_index = None;
                                set_input_line(&mut line, &draft_line, &mut cursor_pos, input_origin, &mut rendered_len);
                            }
                        } else {
                            history_index = None;
                            set_input_line(&mut line, &draft_line, &mut cursor_pos, input_origin, &mut rendered_len);
                        }
                    }
                }
                keyboard::KeyEvent::Enter => {
                    with_console(|c| c.newline());
                    commands::handle_line(&line);
                    history::push(&line);
                    if let Some(next) = run_mode::should_switch() {
                        return next;
                    }
                    line.clear();
                    draft_line.clear();
                    history_index = None;
                    cursor_pos = 0;
                    rendered_len = 0;
                    selection_anchor = None;
                    input_origin = with_console(|c| {
                        let prompt = prompt_text();
                        c.write(&prompt);
                        c.cursor_position()
                    });
                }
                keyboard::KeyEvent::Tab => {
                    if let Some(cmd) = commands::suggest_command(&line) {
                        line.clear();
                        let _ = line.push_str(cmd.as_str());
                        cursor_pos = line.chars().count();
                        redraw_input_line(&line, cursor_pos, input_origin, &mut rendered_len, selection_anchor);
                    }
                }
                keyboard::KeyEvent::AltTab | keyboard::KeyEvent::Start => {}
            }
        }
    }
}

#[allow(dead_code)]
fn delete_selection(line: &mut String<128>, cursor_pos: &mut usize, anchor: usize) {
    let start = anchor.min(*cursor_pos);
    let end = anchor.max(*cursor_pos);
    if start == end { return; }

    let mut new_line = String::<128>::new();
    for (i, ch) in line.chars().enumerate() {
        if i >= start && i < end { continue; }
        let _ = new_line.push(ch);
    }
    *line = new_line;
    *cursor_pos = start;
}

#[allow(dead_code)]
fn insert_char_at(line: &mut String<128>, idx: usize, ch: char) -> bool {
    let len = line.chars().count();
    if idx > len {
        return false;
    }
    let mut new_line = String::<128>::new();
    let mut inserted = false;
    for (i, existing) in line.chars().enumerate() {
        if i == idx {
            if new_line.push(ch).is_err() { return false; }
            inserted = true;
        }
        if new_line.push(existing).is_err() { return false; }
    }
    if !inserted && new_line.push(ch).is_err() {
        return false;
    }
    *line = new_line;
    true
}

#[allow(dead_code)]
fn remove_char_at(line: &mut String<128>, idx: usize) -> bool {
    let len = line.chars().count();
    if idx >= len {
        return false;
    }
    let mut new_line = String::<128>::new();
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

#[allow(dead_code)]
fn delete_prev_word(line: &mut String<128>, cursor_pos: &mut usize) -> bool {
    if *cursor_pos == 0 {
        return false;
    }
    let mut chars = Vec::<char, 128>::new();
    for ch in line.chars() {
        let _ = chars.push(ch);
    }
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

#[allow(dead_code)]
fn move_cursor_word_left(line: &String<128>, cursor_pos: &mut usize) -> bool {
    if *cursor_pos == 0 {
        return false;
    }
    let chars: Vec<char, 128> = line.chars().collect();
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

#[allow(dead_code)]
fn move_cursor_word_right(line: &String<128>, cursor_pos: &mut usize) -> bool {
    let chars: Vec<char, 128> = line.chars().collect();
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

#[allow(dead_code)]
fn set_input_line(
    line: &mut String<128>,
    new_content: &str,
    cursor_pos: &mut usize,
    origin: (usize, usize),
    rendered_len: &mut usize,
) {
    line.clear();
    for ch in new_content.chars() {
        if line.push(ch).is_err() {
            break;
        }
    }
    *cursor_pos = line.chars().count();
    redraw_input_line(line, *cursor_pos, origin, rendered_len, None);
}

#[allow(dead_code)]
fn redraw_input_line(
    line: &String<128>,
    cursor_pos: usize,
    origin: (usize, usize),
    rendered_len: &mut usize,
    selection_anchor: Option<usize>,
) {
    let selection = if let Some(anchor) = selection_anchor {
        let start = anchor.min(cursor_pos);
        let end = anchor.max(cursor_pos);
        if start == end {
             None
        } else {
            Some((start, end))
        }
    } else {
        None
    };
    
    let suggestion_full = commands::suggest_command(line);
    let suggestion_suffix = suggestion_full
        .as_ref()
        .and_then(|s| s.as_str().get(line.len()..));

    let new_len = console::render_line_at(origin.0, origin.1, line.as_str(), *rendered_len, cursor_pos, selection, suggestion_suffix);
    *rendered_len = new_len;
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    with_console(|c| {
        c.write_line("");
        c.cwrite_line("=== KERNEL PANIC ===", 0xFF0000, 0x000000);
        let msg = alloc_str(info);
        c.cwrite_line(&msg, 0xFFFF8F, 0x000000);
        c.write_line("");
        c.cwrite_line("Attempting to fix via reboot...", 0x0047AB, 0x000000);
    });

    crate::commands::wait_ticks(300);

    crate::commands::reboot();

    with_console(|c| {
        c.write_line("Reboot failed! Halting...");
    });

    loop {
        unsafe { x86::halt(); }
    }
}

fn alloc_str(info: &PanicInfo) -> heapless::String<256> {
    use core::fmt::Write;
    let mut s = heapless::String::<256>::new();
    let _ = write!(&mut s, "{info}");
    s
}
