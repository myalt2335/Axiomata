use crate::{cdmo, debug, console, time, serial, wait, history, memory, OS_NAME, OS_VERSION, fs, forth, editor};
use crate::help::{BSOD_HEIGHT, BSOD_IMAGE, BSOD_WIDTH};
use alloc::borrow::ToOwned;
use alloc::string::ToString;
use crate::console::{
    CursorStyle,
    CursorBlink,
    FontKind,
};
use alloc::format;
use heapless::{String as HString, Vec, LinearMap};
use spin::Mutex;
use raw_cpuid::CpuId;
use core::fmt::Write;

#[no_mangle]
pub static mut TICKS: u64 = 0;


pub const COMMANDS: &[&str] = &[
    "help", "about", "os", "echo", "cls", "clear", "uptime", "reboot", "shutdown", "meminfo", 
    "memtest", "cpuinfo", "fbinfo", "version", "alias", "unalias", "aliases", "cecho", "secho",
    "ls", "cd", "pwd", "mkdir", "rmdir", "touch", "cat", "rm", "del", "vight", "forth",
];

static mut CMD_COMPLETION_ENABLED: bool = false;
static mut PROMPT_PATH_ENABLED: bool = false;

pub fn is_completion_enabled() -> bool {
    unsafe { CMD_COMPLETION_ENABLED }
}

pub fn is_prompt_path_enabled() -> bool {
    unsafe { PROMPT_PATH_ENABLED }
}

fn toggle_prompt_path() -> bool {
    unsafe {
        PROMPT_PATH_ENABLED = !PROMPT_PATH_ENABLED;
        PROMPT_PATH_ENABLED
    }
}
fn suggest_alias(prefix: &str) -> Option<HString<32>> {
    let aliases = ALIASES.lock();

    for (alias, _) in aliases.iter() {
        if alias.as_str().starts_with(prefix) {
            return Some(alias.clone());
        }
    }

    None
}

pub fn suggest_command(input: &str) -> Option<HString<32>> {
    if input.is_empty() || !is_completion_enabled() {
        return None;
    }

    if forth::is_active() || editor::is_active() {
        return None;
    }
    
    let input_lower = input.to_ascii_lowercase();

    if let Some(alias) = suggest_alias(&input_lower) {
        return Some(alias);
    }
    
    for cmd in COMMANDS {
        if cmd.starts_with(&input_lower) {
            let mut suggestion = HString::<32>::new();
            if suggestion.push_str(cmd).is_ok() {
                return Some(suggestion);
            }
        }
    }
    None
}

static ALIASES: Mutex<LinearMap<HString<32>, HString<32>, 32>> =
    Mutex::new(LinearMap::new());

pub fn add_alias(alias: &str, command: &str) {
    let mut aliases = ALIASES.lock();

    let alias_lower = alias.to_ascii_lowercase();
    let command_lower = command.to_ascii_lowercase();

    let mut alias_str: HString<32> = HString::new();
    let mut command_str: HString<32> = HString::new();

    if alias_str.push_str(&alias_lower).is_err() || command_str.push_str(&command_lower).is_err() {
        console::write_line("Alias too long (max 32 chars).");
        return;
    }

    if aliases.contains_key(&alias_str) || aliases.values().any(|v| v == &alias_str) {
        console::write_line("Alias already exists or conflicts.");
        return;
    }

    aliases.insert(alias_str.clone(), command_str.clone()).ok();
    console::write_line(&format!("Alias added: {} -> {}", alias_lower, command_lower));
}

pub fn remove_alias(alias: &str) {
    let mut aliases = ALIASES.lock();

    let alias_lower = alias.to_ascii_lowercase();
    let mut alias_str: HString<32> = HString::new();
    let _ = alias_str.push_str(&alias_lower);

    if aliases.remove(&alias_str).is_some() {
        console::write_line(&format!("Alias removed: {}", alias_lower));
    } else {
        console::write_line("Alias not found.");
    }
}

pub fn list_aliases() {
    let aliases = ALIASES.lock();
    if aliases.is_empty() {
        console::write_line("No aliases defined.");
    } else {
        console::write_line("Aliases:");
        for (alias, target) in aliases.iter() {
            console::write_line(&format!("  {} -> {}", alias, target));
        }
    }
}

pub fn resolve_alias(cmd: &str) -> HString<32> {
    let aliases = ALIASES.lock();

    let cmd_lower = cmd.to_ascii_lowercase();
    let mut key: HString<32> = HString::new();
    let _ = key.push_str(&cmd_lower);

    if let Some(target) = aliases.get(&key) {
        target.clone()
    } else {
        key
    }
}

pub fn tick() {
    unsafe { TICKS += 1; }
    cdmo::tick();
}

pub fn wait_ticks(ticks: u64) {
    let start = unsafe { TICKS };
    while unsafe { TICKS } - start < ticks {
        unsafe { x86::halt(); }
    }
}

pub fn echo(args: &[&str]) {
    let mut s = HString::<128>::new();
    for (i, word) in args.iter().enumerate() {
        if i > 0 {
            let _ = s.push(' ');
        }
        let _ = s.push_str(word);
    }
    console::write_line(&s);
}

pub fn secho(args: &[&str]) {
    let mut s: HString<128> = HString::new();

    for (i, word) in args.iter().enumerate() {
        if i > 0 {
            let _ = s.push(' ');
        }

        if s.push_str(word).is_err() {
            serial::write("Error: message too long");
            return;
        }
    }
    serial::write(&s);
}

fn parse_rgb_hex(s: &str) -> Option<u32> {
    let h = s.trim();
    if h.len() == 3 {
        let mut buf = [0u8; 6];
        for (i, b) in h.bytes().enumerate() {
            let hi = b;
            buf[i * 2] = hi;
            buf[i * 2 + 1] = hi;
        }
        let expanded = core::str::from_utf8(&buf).ok()?;
        return u32::from_str_radix(expanded, 16).ok();
    }
    if h.len() == 6 {
        return u32::from_str_radix(h, 16).ok();
    }
    None
}

pub const DESKTOP_PORTING_DISABLED_MSG: &str =
    "This feature is temporarily disabled as we work on porting it to the new desktop.";
const CURSOR_USAGE: &str = "Usage: cursor style underscore|line|block|hidden OR cursor blink none|pulse|fade OR cursor color <hex>";
const FONT_USAGE: &str = "Usage: os font vga8|default|terminus|spleen";
const HUD_USAGE: &str = "Usage: os hud toggle";
const TEXT_USAGE: &str = "Usage: os text <hex>";
const BG_USAGE: &str = "Usage: os bg <hex>";
const CMDHIST_USAGE: &str = "Usage: os cmdhistory clear|toggle";
const CLIPBOARD_USAGE: &str = "Usage: os clipboard clear";
const CMDCMP_USAGE: &str = "Usage: os cmdcmp toggle";
const PRMPTH_USAGE: &str = "Usage: os prmpth toggle";

fn os_usage() {
    console::write_line("Usage: os <font|cursor|hud|text|bg|clipboard|prmpth> ...");
    console::write_line("  font   default/vga8|terminus|spleen");
    console::write_line("  cursor style underscore|line|block|hidden");
    console::write_line("  cursor blink none|pulse|fade");
    console::write_line("  cursor color <hex>");
    console::write_line("  hud    toggle");
    console::write_line("  text   <hex>  (default text color)");
    console::write_line("  bg     <hex>  (default background, clears screen)");
    console::write_line("  cmdhistory clear|toggle");
    console::write_line("  clipboard clear");
    console::write_line("  time   12hr|24hr|sync|log|source|help");

    console::write_line("  cmdcmp toggle");
    console::write_line("  prmpth toggle");
}

fn handle_cursor_args(args: &[&str]) -> Result<(), &'static str> {
    if args.len() < 2 {
        return Err(CURSOR_USAGE);
    }

    let mode = args[0];
    let value = args[1];

    if mode.eq_ignore_ascii_case("style") {
        if value.eq_ignore_ascii_case("underscore") {
            console::set_cursor_style(CursorStyle::Underscore);
        } else if value.eq_ignore_ascii_case("line") {
            console::set_cursor_style(CursorStyle::Line);
        } else if value.eq_ignore_ascii_case("block") {
            console::set_cursor_style(CursorStyle::Block);
        } else if value.eq_ignore_ascii_case("hidden") {
            console::set_cursor_style(CursorStyle::Hidden);
        } else {
            return Err(CURSOR_USAGE);
        }
        Ok(())
    } else if mode.eq_ignore_ascii_case("blink") {
        if value.eq_ignore_ascii_case("none") {
            console::set_cursor_blink(CursorBlink::None);
        } else if value.eq_ignore_ascii_case("pulse") {
            console::set_cursor_blink(CursorBlink::Pulse);
        } else if value.eq_ignore_ascii_case("fade") {
            console::set_cursor_blink(CursorBlink::Fade);
        } else {
            return Err(CURSOR_USAGE);
        }
        Ok(())
    } else if mode.eq_ignore_ascii_case("color") {
        match parse_rgb_hex(value) {
            Some(v) if v <= 0xFFFFFF => {
                console::set_cursor_color(v);
                console::write_line(&format!("Cursor color set to #{:06X}.", v));
                Ok(())
            }
            _ => Err("cursor color: invalid hex. Use 3 or 6 hex digits, e.g., FF00FF"),
        }
    } else {
        Err(CURSOR_USAGE)
    }
}

fn handle_hud_args(args: &[&str]) -> Result<(), &'static str> {
    match args.get(0) {
        Some(state) if state.eq_ignore_ascii_case("toggle") => {
            if crate::thud::is_enabled() {
                crate::thud::disable();
                console::write_line("Terminal HUD disabled.");
            } else {
                crate::thud::enable();
                console::write_line("Terminal HUD enabled.");
            }
            Ok(())
        }
        _ => Err(HUD_USAGE),
    }
}

fn handle_cmdhistory_args(args: &[&str]) -> Result<(), &'static str> {
    match args.get(0) {
        Some(cmd) if cmd.eq_ignore_ascii_case("clear") => {
            history::clear();
            console::write_line("Command history cleared.");
            Ok(())
        }
        Some(cmd) if cmd.eq_ignore_ascii_case("toggle") => {
            let enabled = history::toggle_enabled();
            if enabled {
                console::write_line("Command history enabled.");
            } else {
                console::write_line("Command history disabled and cleared.");
            }
            Ok(())
        }
        _ => Err(CMDHIST_USAGE),
    }
}

fn handle_time_args(args: &[&str]) -> Result<(), &'static str> {
    if args.is_empty() {
        time::time_cmd(&[]);
        return Ok(());
    }

    
    time::time_cmd(args);
    Ok(())
}

fn handle_cmdcmp_args(args: &[&str]) -> Result<(), &'static str> {
    match args.get(0) {
        Some(cmd) if cmd.eq_ignore_ascii_case("toggle") => {
            unsafe {
                CMD_COMPLETION_ENABLED = !CMD_COMPLETION_ENABLED;
                if CMD_COMPLETION_ENABLED {
                    console::write_line("Command completion enabled.");
                } else {
                    console::write_line("Command completion disabled.");
                }
            }
            Ok(())
        }
        _ => Err(CMDCMP_USAGE),
    }
}

fn handle_prmpth_args(args: &[&str]) -> Result<(), &'static str> {
    match args.get(0) {
        Some(cmd) if cmd.eq_ignore_ascii_case("toggle") => {
            let enabled = toggle_prompt_path();
            if enabled {
                console::write_line("Prompt path enabled.");
            } else {
                console::write_line("Prompt path disabled.");
            }
            Ok(())
        }
        _ => Err(PRMPTH_USAGE),
    }
}

fn handle_font_args(args: &[&str]) -> Result<(), &'static str> {
    let Some(name) = args.get(0) else {
        return Err(FONT_USAGE);
    };

    if name.eq_ignore_ascii_case("vga8") || name.eq_ignore_ascii_case("default") {
        console::set_font(FontKind::Vga8);
        console::write_line("Font set to VGA 8x8.");
        Ok(())
    } else if name.eq_ignore_ascii_case("terminus") {
        console::set_font(FontKind::Terminus8x16);
        console::write_line("Font set to Terminus 8x16.");
        Ok(())
    } else if name.eq_ignore_ascii_case("spleen") {
        console::set_font(FontKind::Spleen8x16);
        console::write_line("Font set to Spleen 8x16.");
        Ok(())
    } else {
        Err(FONT_USAGE)
    }
}

pub fn os_command(args: &[&str]) {
    if args.is_empty() {
        os_usage();
        return;
    }

    let sub = args[0].to_ascii_lowercase();
    match sub.as_str() {
        "font" => {
            if let Err(msg) = handle_font_args(&args[1..]) {
                console::write_line(msg);
            }
        }
        "cursor" => {
            if let Err(msg) = handle_cursor_args(&args[1..]) {
                console::write_line(msg);
            }
        }
        "hud" => {
            if let Err(msg) = handle_hud_args(&args[1..]) {
                console::write_line(msg);
            }
        }
        "clipboard" => {
            if args.len() > 1 && args[1].eq_ignore_ascii_case("clear") {
                crate::clipboard::set_text("");
                console::write_line("Clipboard cleared.");
            } else {
                 console::write_line(CLIPBOARD_USAGE);
            }
        }
        "cmdhistory" => {
            if let Err(msg) = handle_cmdhistory_args(&args[1..]) {
                console::write_line(msg);
            }
        }
        "time" => {
            if let Err(msg) = handle_time_args(&args[1..]) {
                console::write_line(msg);
            }
        }
        "cmdcmp" => {
            if let Err(msg) = handle_cmdcmp_args(&args[1..]) {
                console::write_line(msg);
            }
        }
        "prmpth" => {
            if let Err(msg) = handle_prmpth_args(&args[1..]) {
                console::write_line(msg);
            }
        }
        "text" => {
            match args.get(1) {
                Some(code) => match parse_rgb_hex(code) {
                    Some(v) if v <= 0xFFFFFF => {
                        console::set_default_fg(v);
                        console::write_line(&format!("Default text color set to #{:06X}.", v));
                    }
                    _ => console::write_line("os text: invalid hex. Use 3 or 6 hex digits, e.g., FF0000"),
                },
                None => console::write_line(TEXT_USAGE),
            }
        }
        "bg" => {
            match args.get(1) {
                Some(code) => match parse_rgb_hex(code) {
                    Some(v) if v <= 0xFFFFFF => {
                        let prev = console::default_bg();
                        console::set_default_bg(v);
                        if v != prev {
                            console::write_line(&format!("Default background set to #{:06X}. Screen cleared.", v));
                        } else {
                            console::write_line(&format!("Default background remains #{:06X}.", v));
                        }
                    }
                    _ => console::write_line("os bg: invalid hex. Use 3 or 6 hex digits, e.g., 000000"),
                },
                None => console::write_line(BG_USAGE),
            }
        }
        "help" => os_usage(),
        _ => os_usage(),
    }
}

pub fn cecho(args: &[&str]) {
    if args.len() < 2 {
        console::write_line("Usage: cecho <hex> <text>");
        return;
    }

    let fg = match parse_rgb_hex(args[0]) {
        Some(v) if v <= 0xFFFFFF => v,
        _ => {
            console::write_line("cecho: invalid hex. Use 3 or 6 hex digits, e.g., FF0000");
            return;
        }
    };

    let mut s = HString::<128>::new();
    for (i, word) in args[1..].iter().enumerate() {
        if i > 0 { let _ = s.push(' '); }
        let _ = s.push_str(word);
    }

    console::cwrite_line(&s, fg, console::default_bg());
}

fn bytobi(input: &str) -> Option<u32> {
    use heapless::String;

    let mut num_str: String<16> = String::new();

    for c in input.chars() {
        if c.is_ascii_digit() {
            num_str.push(c).ok()?;
        } else {
            break;
        }
    }

    if num_str.is_empty() {
        return None;
    }

    let bytes: u32 = num_str.parse().ok()?;
    Some(bytes * 8)
}

pub fn fbtst() {
    console::with_console(|c| {
        let info = c.framebuffer_info();
        
        let width = info.width;
        let height = info.height;
        let bytes_per_pixel = info.bytes_per_pixel;
        let stride = info.stride;
        let pixel_format = info.pixel_format;

        use heapless::String;
        use core::fmt::Write;

        let mut bpp_str: String<8> = String::new();
        write!(&mut bpp_str, "{}", bytes_per_pixel).ok();

        let bits_per_pixel: u32 =
            bytobi(&bpp_str).unwrap_or((bytes_per_pixel as u32) * 8);

        c.write_line("Framebuffer Info:");
        c.write_line(&format!("  Width: {}", width));
        c.write_line(&format!("  Height: {}", height));
        c.write_line(&format!("  Bits per pixel: {}", bits_per_pixel));
        c.write_line(&format!("  Stride (px): {}", stride));
        c.write_line(&format!("  PixelFormat: {:?}", pixel_format));
    });
}

pub fn clear() {
    console::clear_text_area();
}

pub fn help(args: &[&str]) {
    if let Some(topic) = args.get(0) {
        let topic = topic.to_ascii_lowercase();
        let msg = match topic.as_str() {
            "help" => "help shows available commands. Usage: help [command]",
            "about" => "Prints info about Axiomata and your hardware.",
            "os" => "Changes system settings (font, cursor, HUD, colors, cmdhistory, time, prompt). Usage: os <subcommand> ...",
            "echo" => "Prints text to the console. Usage: echo <text>",
            "cecho" => "Prints colored text. Usage: cecho <hex> <text> (hex in RGB, e.g., FF00FF)",
            "secho" => "Writes text to the serial port. Usage: secho <text>",
            "clear" => "Clears the screen.",
            "uptime" => "Shows how long the system has been running since boot.",
            "reboot" => "Restarts the device.",
            "shutdown" => "Attempts to turn off the device.",
            "meminfo" => "Shows memory statistics (total, reserved, free).",
            "memtest" => "Runs the built-in memory test.",
            "cpuinfo" => "Lists CPU vendor/brand/features if available.",
            "fbinfo" => "Shows framebuffer dimensions, bpp, stride, and format.",
            "version" => "Prints Axiomata name and build version.",
            "alias" => "Creates an alias. Usage: alias <command> <alias>",
            "unalias" => "Removes an alias. Usage: unalias <alias>",
            "aliases" => "Lists all defined aliases.",
            "ls" => "Lists files and folders. Usage: ls [path]",
            "cd" => "Changes the current directory. Usage: cd <path>",
            "pwd" => "Prints the current directory.",
            "mkdir" => "Creates a folder. Usage: mkdir <path>",
            "rmdir" => "Removes an empty folder. Usage: rmdir <path>",
            "touch" => "Creates an empty file. Usage: touch <path>",
            "cat" => "Prints file contents. Usage: cat <path>",
            "rm" => "Deletes a file. Usage: rm <path>",
            "fsinfo" => "Shows persistent filesystem status.",
            "fstype" => "Sets preferred filesystem for mounting/formatting. Usage: fstype [auto|fat32|ext2]",
            "format" => "Formats the selected disk as FAT32 or EXT2. Usage: format [fat32|ext2]",
            "vight" => "Opens the Vight editor (save-as, :find, :status, :reload, :q!). Usage: vight <name>",
            "forth" => "Runs Forth. Usage: forth           (REPL) | forth <file.f> (run-and-quit) | forth examples (install demo .f files)",
            _ => {
                console::write_line("Unknown command for help.");
                return;
            }
        };
        console::write_line(msg);
        return;
    }

    console::write_line("\nAvailable commands (type 'help <command>' for details):");
    console::write_line("  help          - Show this help or per-command details");
    console::write_line("  about         - Show Axiomata build and system summary");
    console::write_line("  os ...        - System settings");
    console::write_line("  echo <text>   - Print text");
    console::write_line("  clear         - Clear the screen");
    console::write_line("  uptime        - Show uptime since boot");
    console::write_line("  reboot        - Reboot the machine");
    console::write_line("  shutdown      - Power down the machine");
    console::write_line("  meminfo       - Show memory info");
    console::write_line("  memtest       - Test the memory");
    console::write_line("  cpuinfo       - Show CPU info");
    console::write_line("  fbinfo        - Show framebuffer info");
    console::write_line("  version       - Show OS version");
    console::write_line("  alias         - Create an alias");
    console::write_line("  unalias       - Remove an alias");
    console::write_line("  aliases       - List all aliases");
    console::write_line("  ls [path]     - List directory entries");
    console::write_line("  cd <path>     - Change directory");
    console::write_line("  pwd           - Print current directory");
    console::write_line("  mkdir <path>  - Create a folder");
    console::write_line("  rmdir <path>  - Remove an empty folder");
    console::write_line("  touch <path>  - Create an empty file");
    console::write_line("  cat <path>    - Print a file");
    console::write_line("  rm <path>     - Delete a file");
    console::write_line("  fsinfo        - Filesystem persistence status");
    console::write_line("  fstype        - Set preferred filesystem (auto/fat32/ext2)");
    console::write_line("  format        - Format selected disk as FAT32 or EXT2");
    console::write_line("  vight <name>  - Edit a file (:w, :q!, :find, :status)");
    console::write_line("  forth [...]   - Forth REPL (no args), run file.f, or 'examples'\n");
}
















pub fn about() {
    console::write_line("Axiomata Codename Conjecture");
    console::write_line(&format!("Version: {}", OS_VERSION));
    console::write_line("Built with Rust.");

    console::write_line("");
    console::write_line("System:");
    let cpuid = CpuId::new();
    let cpu_line = if let Some(brand) = cpuid.get_processor_brand_string() {
        let vendor = cpuid
            .get_vendor_info()
            .map(|v| v.as_str().to_owned())
            .unwrap_or_else(|| "Unknown".to_string());
        format!("  CPU: {} ({})", brand.as_str().trim(), vendor)
    } else if let Some(vendor) = cpuid.get_vendor_info() {
        format!("  CPU: {}", vendor.as_str())
    } else {
        "  CPU: Unknown".into()
    };
    console::write_line(&cpu_line);

    let stats = memory::system_stats();
    console::write_line(&format!(
        "  Memory: total {} KB, reserved {} KB, free {} KB",
        stats.total / 1024,
        stats.reserved / 1024,
        stats.free / 1024
    ));

    console::write_line("");
    console::write_line("Time:");
    time::time_cmd(&[]);
    uptime();
}

pub fn uptime() {
    unsafe {
        let secs = TICKS / 100;
        let mins = secs / 60;
        let hours = mins / 60;
        console::write_line(&format!(
            "Uptime: {:02}:{:02}:{:02}",
            hours,
            mins % 60,
            secs % 60
        ));
    }
}

pub fn version() {
    console::write_line(OS_NAME);
    console::write_line(&format!("Shell {}", OS_VERSION));
    console::write_line("");
    console::write_line("Built with Rust.");
}

pub fn reboot() {
    console::write_line("Attempting to reboot...");
    wait_ticks(20);

    let ok = unsafe {
        x86::io::outb(0x64, 0xFE);
        true
    };

    if !ok {
        console::write_line("Something went wrong, attempting to restart the machine\n");
    }
}

pub fn shutdown() -> ! {
    console::write_line("Attempting to shut down...");

    let ok = unsafe {
        x86::io::outw(0x604, 0x2000);
        true
    };

    if !ok {
        console::write_line("\nSomething went wrong attempting to shut down the machine.");
        console::write_line("Halting to allow for safe machine shutdown....\n");
    }

    loop {
        unsafe { x86::halt(); }
    }
}

fn format_bytes<const N: usize>(bytes: usize) -> HString<N> {
    let mut s: HString<N> = HString::new();
    const KB: usize = 1024;
    const MB: usize = KB * 1024;
    const GB: usize = MB * 1024;

    if bytes >= GB {
        let _ = write!(s, "{:.1} GiB", bytes as f64 / GB as f64);
    } else if bytes >= MB {
        let _ = write!(s, "{:.1} MiB", bytes as f64 / MB as f64);
    } else if bytes >= KB {
        let _ = write!(s, "{:.1} KiB", bytes as f64 / KB as f64);
    } else {
        let _ = write!(s, "{} B", bytes);
    }
    s
}

pub fn mem_selftest() {
    use crate::memory::{
        register_app, unregister_app, app_alloc, app_dealloc,
        app_stats, kalloc, kdealloc,
    };
    use crate::console;

    const DUMMY_APP: u32 = 42;

    console::write_line("=== Memory self-test starting ===");

    unsafe {
        let p = kalloc(128, 8);
        if !p.is_null() {
            console::write_line("Kernel alloc: success (128 B)");
            kdealloc(p, 128, 8);
            console::write_line("Kernel dealloc: success");
        } else {
            console::write_line("Kernel alloc FAILED");
        }
    }

    if register_app(DUMMY_APP, 64 * 1024) {
        unsafe {
            let ptr = app_alloc(DUMMY_APP, 4096, 8);
            if !ptr.is_null() {
                console::write_line("App alloc: success (4 KiB)");
                app_dealloc(DUMMY_APP, ptr, 4096, 8);
                console::write_line("App dealloc: success");
            } else {
                console::write_line("App alloc FAILED");
            }
        }

        if let Some(stats) = app_stats(DUMMY_APP) {
            console::write_line(&format!(
                "App {} stats: total={} used={} free={} allocs={} deallocs={}",
                DUMMY_APP,
                stats.total, stats.used, stats.free,
                stats.alloc_count, stats.dealloc_count
            ));
        }

        unregister_app(DUMMY_APP);
    } else {
        console::write_line("App register FAILED");
    }

    console::write_line("=== Memory self-test complete ===");
}

pub fn meminfo() {
    use crate::memory::memory_overview;
    use crate::console;

    let mo = memory_overview();

    console::write_line(&format!(
        "System memory:\n  Total: {}\n  Reserved: {}\n  Free: {}",
        format_bytes::<32>(mo.system.total),
        format_bytes::<32>(mo.system.reserved),
        format_bytes::<32>(mo.system.free),
    ));

    console::write_line(&format!(
        "\nKernel heap:\n  Total: {}\n  Used: {}\n  Free: {}\n  Peak: {}\n  Allocs: {}\n  Deallocs: {}",
        format_bytes::<32>(mo.kernel_heap.total),
        format_bytes::<32>(mo.kernel_heap.used),
        format_bytes::<32>(mo.kernel_heap.free),
        format_bytes::<32>(mo.kernel_heap.peak_used),
        mo.kernel_heap.alloc_count,
        mo.kernel_heap.dealloc_count,
    ));

    console::write_line(&format!(
        "\nUser arena:\n  Total: {}\n  Free for new regions: {}",
        format_bytes::<32>(mo.user_arena_total),
        format_bytes::<32>(mo.user_arena_free_for_new_regions),
    ));

    if let Some(db) = mo.display {
        console::write_line(&format!(
            "\nDisplay buffers:\n  Resolution: {}x{} (stride {} px, {} Bpp)\n  Framebuffer: {} ({} B)\n  Back buffer: {} ({} B)",
            db.width_px,
            db.height_px,
            db.stride_px,
            db.bytes_per_pixel,
            format_bytes::<32>(db.framebuffer_bytes),
            db.framebuffer_bytes,
            format_bytes::<32>(db.backbuffer_bytes),
            db.backbuffer_bytes,
        ));
    } else {
        console::write_line("\nDisplay buffers:\n  Not initialized");
    }

    for e in mo.apps.iter().flatten() {
        let (id, st) = *e;
        console::write_line(&format!(
            "\nApp {}:\n  Total: {}\n  Used: {}\n  Free: {}\n  Peak: {}\n  Allocs: {}\n  Deallocs: {}",
            id,
            format_bytes::<32>(st.total),
            format_bytes::<32>(st.used),
            format_bytes::<32>(st.free),
            format_bytes::<32>(st.peak_used),
            st.alloc_count,
            st.dealloc_count,
        ));
    }
}

pub fn cpuinfo() {
    let cpuid = CpuId::new();

    if let Some(vf) = cpuid.get_vendor_info() {
        console::write_line(&format!("CPU Vendor: {}", vf.as_str()));
    }

    if let Some(fi) = cpuid.get_feature_info() {
        console::write_line(&format!(
            "Model: {} Family: {} Stepping: {}",
            fi.model_id(),
            fi.family_id(),
            fi.stepping_id()
        ));
        console::write_line(&format!(
            "Features: SSE={} SSE2={} SSE3={} AVX={}",
            fi.has_sse(),
            fi.has_sse2(),
            fi.has_sse3(),
            fi.has_avx()
        ));
    }

    if let Some(pf) = cpuid.get_processor_brand_string() {
        let brand: &str = pf.as_str();
        console::write_line(&format!("Brand: {}", brand));
    }
}

fn ramfs_ls(args: &[&str]) {
    let entries = if let Some(path) = args.get(0) {
        match fs::list_dir(path) {
            Ok(entries) => entries,
            Err(e) => {
                console::write_line(e);
                return;
            }
        }
    } else {
        fs::list_files()
    };
    if entries.is_empty() {
        if args.is_empty() && fs::current_dir() == "\\" {
            console::write_line("Filesystem is empty.");
        } else {
            console::write_line("Directory is empty.");
        }
        return;
    }

    console::write_line(" Size    Name");
    for e in entries {
        if e.is_dir {
            console::write_line(&format!("{:>6}  {}\\", "<DIR>", e.name));
        } else {
            console::write_line(&format!("{:>6}  {}", e.size, e.name));
        }
    }
}

fn ramfs_cd(args: &[&str]) {
    let Some(path) = args.get(0) else {
        console::write_line("Usage: cd <path>");
        return;
    };

    if let Err(e) = fs::set_current_dir(path) {
        console::write_line(e);
    }
}

fn ramfs_pwd() {
    console::write_line(&fs::display_cwd());
}

fn ramfs_mkdir(args: &[&str]) {
    let Some(path) = args.get(0) else {
        console::write_line("Usage: mkdir <path>");
        return;
    };

    match fs::mkdir(path) {
        Ok(_) => console::write_line(&format!("Directory ready: {}", path)),
        Err(e) => console::write_line(e),
    }
}

fn ramfs_rmdir(args: &[&str]) {
    let Some(path) = args.get(0) else {
        console::write_line("Usage: rmdir <path>");
        return;
    };

    match fs::rmdir(path) {
        Ok(_) => console::write_line(&format!("Deleted {}", path)),
        Err(e) => console::write_line(e),
    }
}

fn ramfs_touch(args: &[&str]) {
    let Some(name) = args.get(0) else {
        console::write_line("Usage: touch <path>");
        return;
    };

    match fs::touch(name) {
        Ok(_) => console::write_line(&format!("File ready: {}", name)),
        Err("File already exists.") => console::write_line("File already exists."),
        Err(e) => console::write_line(e),
    }
}

fn ramfs_cat(args: &[&str]) {
    let Some(name) = args.get(0) else {
        console::write_line("Usage: cat <path>");
        return;
    };

    match fs::read_file(name) {
        Some(body) => {
            if body.is_empty() {
                console::write_line("(empty file)");
            } else {
                for line in body.lines() {
                    console::write_line(line);
                }
            }
        }
        None => console::write_line("File not found."),
    }
}

fn ramfs_rm(args: &[&str]) {
    let Some(name) = args.get(0) else {
        console::write_line("Usage: rm <path>");
        return;
    };

    match fs::delete_file(name) {
        Ok(_) => console::write_line(&format!("Deleted {}", name)),
        Err(e) => console::write_line(e),
    }
}

fn fs_kind_label(kind: Option<fs::FsKind>) -> &'static str {
    match kind {
        Some(fs::FsKind::Fat32) => "fat32",
        Some(fs::FsKind::Ext2) => "ext2",
        None => "none",
    }
}

fn fs_pref_label(pref: fs::FsPreference) -> &'static str {
    match pref {
        fs::FsPreference::Auto => "auto",
        fs::FsPreference::Fat32 => "fat32",
        fs::FsPreference::Ext2 => "ext2",
    }
}

fn fs_status() {
    let info = fs::persist_info();
    if info.drive.is_none() {
        console::write_line("Persistent filesystem: no disk selected.");
        console::write_line(&format!(
            "Filesystem preference: {}.",
            fs_pref_label(info.preferred_fs)
        ));
        return;
    }

    if info.enabled {
        console::write_line("Persistent filesystem: enabled.");
    } else {
        console::write_line("Persistent filesystem: disabled.");
    }
    console::write_line(&format!(
        "Filesystem type: {} (preferred {}).",
        fs_kind_label(info.fs_kind),
        fs_pref_label(info.preferred_fs)
    ));

    if let Some(fat) = info.fat32_info {
        let label = core::str::from_utf8(&fat.volume_label)
            .unwrap_or("NO_LABEL")
            .trim();
        console::write_line(&format!("FAT32 label: '{}'.", label));
    }
    if let Some(ext2) = info.ext2_info {
        let label = core::str::from_utf8(&ext2.volume_name)
            .unwrap_or("NO_LABEL")
            .trim();
        console::write_line(&format!(
            "EXT2 label: '{}' (block {} bytes).",
            label, ext2.block_size
        ));
    }

    fn to_usize(bytes: u64) -> usize {
        if bytes > usize::MAX as u64 {
            usize::MAX
        } else {
            bytes as usize
        }
    }

    let total_bytes = info.sectors.saturating_mul(512) as u64;
    if info.enabled {
        match fs::usage_info() {
            Ok(usage) => {
                let total = format_bytes::<32>(to_usize(usage.total_bytes));
                let used = format_bytes::<32>(to_usize(usage.used_bytes));
                let free = format_bytes::<32>(to_usize(usage.free_bytes));
                console::write_line(&format!(
                    "Storage: {} total, {} used, {} free ({}% used).",
                    total, used, free, usage.used_percent
                ));
            }
            Err(_) => {
                let total = format_bytes::<32>(to_usize(total_bytes));
                console::write_line(&format!("Storage: {} total.", total));
            }
        }
    } else {
        let total = format_bytes::<32>(to_usize(total_bytes));
        console::write_line(&format!("Storage: {} total.", total));
    }
}

fn fs_type_cmd(args: &[&str]) {
    if args.is_empty() {
        let info = fs::persist_info();
        console::write_line(&format!(
            "Filesystem: {} (preferred {}).",
            fs_kind_label(info.fs_kind),
            fs_pref_label(info.preferred_fs)
        ));
        return;
    }

    let pref = match args[0].to_ascii_lowercase().as_str() {
        "auto" => fs::FsPreference::Auto,
        "fat32" => fs::FsPreference::Fat32,
        "ext2" => fs::FsPreference::Ext2,
        _ => {
            console::write_line("Usage: fstype [auto|fat32|ext2]");
            return;
        }
    };
    fs::set_fs_preference(pref);
    fs::init_persistent();
    console::write_line(&format!(
        "Filesystem preference set to {}.",
        fs_pref_label(pref)
    ));
}

fn fs_format(args: &[&str]) {
    let target = match args.get(0).map(|s| s.to_ascii_lowercase()) {
        None => None,
        Some(name) if name == "fat32" => Some(fs::FsKind::Fat32),
        Some(name) if name == "ext2" => Some(fs::FsKind::Ext2),
        Some(_) => {
            console::write_line("Usage: format [fat32|ext2]");
            return;
        }
    };

    match fs::format_disk(target) {
        Ok(_) => {
            let info = fs::persist_info();
            console::write_line(&format!(
                "Formatted disk as {}.",
                fs_kind_label(info.fs_kind)
            ));
        }
        Err(e) => console::write_line(e),
    }
}

fn vight_command(args: &[&str]) {
    let Some(name) = args.get(0) else {
        console::write_line("Usage: vight <file>");
        return;
    };
    editor::start(name);
}

fn forth_command(args: &[&str]) {
    if let Some(&"examples") = args.get(0) {
        forth::install_examples();
        return;
    }

    if let Some(path) = args.get(0) {
        forth::run_file_once(path);
        return;
    }

    forth::start_repl();
}

pub fn makel(args: &[&str]) {
    if args.len() == 1 && args[0] == "love" {
        console::write_line("Not war?");
    } else {
        console::write_line("Unknown command: make");
    }
}

pub fn halt_cmd(args: &[&str]) {
    if args.len() == 1 && args[0] == "yes-i-know" {
        console::write_line("System halted.");
        loop {
            unsafe { x86::halt(); }
        }
    } else {
        console::write_line("Refusing to halt. Use: halt yes-i-know");
    }
}

pub fn validation(args: &[&str]) {
    let Some(cmd) = args.get(0) else {
        console::write_line("?");
        return;
    };

    match *cmd {
        "me" => match args.get(1) {
            Some(&"pls") => console::write_line("You are a very good boy"),
            _ => console::write_line("Good boy."),
        },

        "you" => console::write_line("I am a good OS"),

        _ => console::write_line("?"),
    }
}

use x86_64::{
    instructions::{interrupts, tables::lidt},
    structures::idt::InterruptDescriptorTable,
    VirtAddr,
};
use core::arch::asm;
use x86_64::structures::DescriptorTablePointer;

#[allow(unused_unsafe)]
#[allow(static_mut_refs)]
pub fn panic_cmd(args: &[&str]) {
    match args {
        ["yes-i-know", "controlled"] => {
            panic!("Kernel panic manually triggered from shell");
        }

        ["yes-i-know", "int4"] => {
            console::write_line("Testing INT4 response...");
            unsafe { asm!("int $4"); }
        }

        ["yes-i-know", "badmem"] => {
            unsafe { interrupts::disable(); }
            console::write_line("Corrupting memory to trigger kernel panic...");

            unsafe {
                let invalid_ptr = 0xffff_ffff_ffff_f000 as *mut u64;
                core::ptr::write_volatile(invalid_ptr, 0xDEADBEEFDEADBEEF);
            }
        }

        ["yes-i-know", "delidt"] => {
            unsafe {
                interrupts::disable();
                console::write_line("Deleting IDT then faulting...");

                let null_idt = DescriptorTablePointer {
                    base: VirtAddr::new(0),
                    limit: 0,
                };
                lidt(&null_idt);

                asm!("ud2", options(noreturn));
            }
        }

        ["yes-i-know", "nullidt"] => {
            unsafe {
                console::write_line("Loading empty IDT then faulting...");
                wait::bsec(1);
                interrupts::disable();

                #[allow(static_mut_refs)]
                static mut EMPTY_IDT: InterruptDescriptorTable = InterruptDescriptorTable::new();
                unsafe { EMPTY_IDT.load(); }

                asm!("int3", options(noreturn));
            }
        }

        ["yes-i-know", "int3"] => {
        unsafe {
            console::write_line("Testing INT3 response... (check serial)");
            asm!("int3");
        }
    }

        ["yes-i-know", "int3andkill"] => {
            unsafe {
                console::write_line("int3'ing to #UD");
                wait::bms(400);
                asm!("int3", options(noreturn));
            }
        }

        ["yes-i-know", "divby0"] => {
            unsafe {
                console::write_line("Dividing by 0...");
                wait::bms(400);
                asm!("xor rax, rax; div rax", options(noreturn));
            }
        }

        ["yes-i-know", "bsod"] => {
            console::showimage(&BSOD_IMAGE, BSOD_WIDTH, BSOD_HEIGHT, 10);
        }

        ["yes-i-know", "ud"] => {
            unsafe {
                console::write_line("Attempting to trigger #UD...");
                wait::bms(400);
                asm!("ud2");
            }
        }

        _ => {
            console::write_line("Usage: panic yes-i-know [controlled|badmem|delidt|nullidt|int3|int3andkill|divby0|ud]");
        }
    }
}

pub fn handle_command(input: &str) {
    let mut parts: Vec<&str, 16> = Vec::new();
    for word in input.split_whitespace() {
        let _ = parts.push(word);
    }

    if parts.is_empty() {
        return;
    }

    let command = resolve_alias(&parts[0]).to_ascii_lowercase();

    if command == "debug" && !debug::is_available() {
        console::write_line(&format!("Unknown command: {}", parts[0]));
        return;
    }

    match command.as_str() {
        "echo" => echo(&parts[1..]),
        "cecho" => cecho(&parts[1..]),
        "secho" => secho(&parts[1..]),
        "version" => version(),
        "about" => about(),
        "make" => makel(&parts[1..]),
        "validate" => validation(&parts[1..]),
        "c418" => console::write_line("Droopy Likes Your Face"),
        "clear" => clear(),
        "cls" => clear(),
        "help" => help(&parts[1..]),
        "xyzzy" => {
            console::write_line("Three times as much happens.");
        }
        "os" => os_command(&parts[1..]),
        "uptime" => uptime(),
        "reboot" => reboot(),
        "fbinfo" => fbtst(),
        "shutdown" => shutdown(),
        "meminfo" => meminfo(),
        "memtest" => mem_selftest(),
        "cpuinfo" => cpuinfo(),
        "ls" => ramfs_ls(&parts[1..]),
        "cd" => ramfs_cd(&parts[1..]),
        "pwd" => ramfs_pwd(),
        "mkdir" => ramfs_mkdir(&parts[1..]),
        "rmdir" => ramfs_rmdir(&parts[1..]),
        "touch" => ramfs_touch(&parts[1..]),
        "cat" => ramfs_cat(&parts[1..]),
        "rm" => ramfs_rm(&parts[1..]),
        "del" => ramfs_rm(&parts[1..]),
        "fsinfo" => fs_status(),
        "fstype" => fs_type_cmd(&parts[1..]),
        "format" => fs_format(&parts[1..]),
        "vight" => vight_command(&parts[1..]),
        "v"=> vight_command(&parts[1..]),
        "forth" => forth_command(&parts[1..]),
        "halt" => halt_cmd(&parts[1..]),
        "panic" => panic_cmd(&parts[1..]),
        "alias" => {
            if parts.len() == 3 {
                add_alias(parts[2], parts[1]);
            } else {
                console::write_line("Usage: alias <command> <alias>");
            }
        }
        "unalias" => {
            if parts.len() == 2 {
                remove_alias(parts[1]);
            } else {
                console::write_line("Usage: unalias <alias>");
            }
        }
        "aliases" => list_aliases(),
        "debug" => debug::command(&parts[1..]),

        _ => console::write_line(&format!("Unknown command: {}", parts[0])),
    }
}

// oui oui cigarette break
fn split_deuxand(line: &str) -> heapless::Vec<heapless::String<128>, 16> {
    use heapless::{String as HString, Vec};

    let mut result: Vec<HString<128>, 16> = Vec::new();
    let mut current = HString::<128>::new();

    let mut in_single = false;
    let mut in_double = false;
    let mut chars = line.chars().peekable();

    while let Some(c) = chars.next() {
        match c {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '&' if !in_single && !in_double => {
                if chars.peek() == Some(&'&') {
                    chars.next();
                    let seg = current.trim();
                    if !seg.is_empty() {
                        let mut s = HString::<128>::new();
                        let _ = s.push_str(seg);
                        let _ = result.push(s);
                    }
                    current.clear();
                    continue;
                } else {
                    let _ = current.push(c);
                }
            }
            other => {
                let _ = current.push(other);
            }
        }
    }

    let seg = current.trim();
    if !seg.is_empty() {
        let mut s = HString::<128>::new();
        let _ = s.push_str(seg);
        let _ = result.push(s);
    }

    result
}

pub fn handle_line(input: &str) {
    if editor::handle_input(input) {
        return;
    }

    if forth::handle_input(input) {
        return;
    }

    let cmds = split_deuxand(input);
    for cmd in cmds {
        handle_command(&cmd);
        if editor::is_active() || forth::is_active() {
            break;
        }
    }
}
