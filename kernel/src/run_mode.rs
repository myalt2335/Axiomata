use core::sync::atomic::{AtomicU8, Ordering};

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum RunMode {
    Console = 0,
    Desktop = 1,
}

static CURRENT_MODE: AtomicU8 = AtomicU8::new(RunMode::Console as u8);
static TARGET_MODE: AtomicU8 = AtomicU8::new(RunMode::Console as u8);

fn mode_from_u8(value: u8) -> RunMode {
    if value == RunMode::Desktop as u8 {
        RunMode::Desktop
    } else {
        RunMode::Console
    }
}

pub fn init(mode: RunMode) {
    CURRENT_MODE.store(mode as u8, Ordering::Release);
    TARGET_MODE.store(mode as u8, Ordering::Release);
}

pub fn enter(mode: RunMode) {
    CURRENT_MODE.store(mode as u8, Ordering::Release);
}

pub fn current() -> RunMode {
    mode_from_u8(CURRENT_MODE.load(Ordering::Acquire))
}

pub fn target() -> RunMode {
    mode_from_u8(TARGET_MODE.load(Ordering::Acquire))
}

pub fn request(mode: RunMode) {
    TARGET_MODE.store(mode as u8, Ordering::Release);
}

#[allow(dead_code)]
pub fn request_toggle() -> RunMode {
    let next = match current() {
        RunMode::Console => RunMode::Desktop,
        RunMode::Desktop => RunMode::Console,
    };
    request(next);
    next
}

pub fn should_switch() -> Option<RunMode> {
    let current = current();
    let target = target();
    if current == target {
        None
    } else {
        Some(target)
    }
}
