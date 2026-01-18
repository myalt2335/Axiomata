use spin::Mutex;
use x86_64::instructions::interrupts;
use x86_64::instructions::port::Port;
use crate::ps2;

const STATUS_PORT: u16 = 0x64;
const DATA_PORT: u16 = 0x60;

const MOUSE_SET_DEFAULTS: u8 = 0xF6;
const MOUSE_ENABLE_DATA: u8 = 0xF4;
const MOUSE_ACK: u8 = 0xFA;
const MOUSE_SET_SAMPLE: u8 = 0xF3;
const MOUSE_GET_ID: u8 = 0xF2;

const STATUS_OUT_FULL: u8 = 0x01;
const STATUS_AUX_DATA: u8 = 0x20;

#[derive(Copy, Clone)]
struct MouseState {
    x: i32,
    y: i32,
    max_x: i32,
    max_y: i32,
    packet: [u8; 4],
    packet_idx: usize,
    packet_len: usize,
    buttons: u8,
    wheel_delta: i32,
    enabled: bool,
}

impl MouseState {
    const fn new() -> Self {
        Self {
            x: 0,
            y: 0,
            max_x: 0,
            max_y: 0,
            packet: [0; 4],
            packet_idx: 0,
            packet_len: 3,
            buttons: 0,
            wheel_delta: 0,
            enabled: false,
        }
    }
}

static STATE: Mutex<MouseState> = Mutex::new(MouseState::new());

fn with_state<F, R>(f: F) -> R
where
    F: FnOnce(&mut MouseState) -> R,
{
    interrupts::without_interrupts(|| {
        let mut state = STATE.lock();
        f(&mut state)
    })
}

fn with_state_ref<F, R>(f: F) -> R
where
    F: FnOnce(&MouseState) -> R,
{
    interrupts::without_interrupts(|| {
        let state = STATE.lock();
        f(&state)
    })
}

pub fn init() -> bool {
    ps2::flush_output();
    let ok_defaults = ps2::send_mouse_command(MOUSE_SET_DEFAULTS) == Some(MOUSE_ACK);
    let wheel_ok = enable_wheel();
    let mut id = if wheel_ok { read_device_id() } else { None };
    let ok_enable = ps2::send_mouse_command(MOUSE_ENABLE_DATA) == Some(MOUSE_ACK);
    if wheel_ok && ok_enable && matches!(id, None | Some(0x00)) {
        id = read_device_id();
    }
    let packet_len = if matches!(id, Some(0x03) | Some(0x04)) { 4 } else { 3 };
    let ok = ok_defaults && ok_enable;
    with_state(|state| {
        state.enabled = ok;
        state.packet_idx = 0;
        state.packet_len = packet_len;
        state.wheel_delta = 0;
    });
    ps2::flush_output();
    ok
}

fn enable_wheel() -> bool {
    set_sample_rate(200) && set_sample_rate(100) && set_sample_rate(80)
}

fn set_sample_rate(rate: u8) -> bool {
    ps2::send_mouse_command(MOUSE_SET_SAMPLE) == Some(MOUSE_ACK)
        && ps2::send_mouse_command(rate) == Some(MOUSE_ACK)
}

fn read_device_id() -> Option<u8> {
    if ps2::send_mouse_command(MOUSE_GET_ID) != Some(MOUSE_ACK) {
        return None;
    }
    for _ in 0..100_000 {
        if let Some(byte) = ps2::read_output_byte() {
            return Some(byte);
        }
    }
    None
}

pub fn set_bounds(width: usize, height: usize) {
    with_state(|state| {
        state.max_x = width.saturating_sub(1) as i32;
        state.max_y = height.saturating_sub(1) as i32;
        if state.x > state.max_x {
            state.x = state.max_x;
        }
        if state.y > state.max_y {
            state.y = state.max_y;
        }
    });
}

pub fn set_position(x: usize, y: usize) {
    with_state(|state| {
        state.x = clamp_i32(x as i32, 0, state.max_x);
        state.y = clamp_i32(y as i32, 0, state.max_y);
    });
}

pub fn position() -> (usize, usize) {
    with_state_ref(|state| (state.x.max(0) as usize, state.y.max(0) as usize))
}

pub fn buttons() -> u8 {
    with_state_ref(|state| state.buttons)
}

pub fn take_wheel_delta() -> i32 {
    with_state(|state| {
        let delta = state.wheel_delta;
        state.wheel_delta = 0;
        delta
    })
}

pub fn poll() -> bool {
    let mut any = false;
    loop {
        let Some(byte) = read_mouse_byte() else { break; };
        if with_state(|state| push_byte_inner(state, byte)) {
            any = true;
        }
    }
    any
}

pub(crate) fn push_byte(byte: u8) {
    let _ = with_state(|state| push_byte_inner(state, byte));
}

fn push_byte_inner(state: &mut MouseState, byte: u8) -> bool {
    if !state.enabled {
        return false;
    }
    if state.packet_idx == 0 && (byte & 0x08) == 0 {
        return false;
    }
    let idx = state.packet_idx;
    state.packet[idx] = byte;
    state.packet_idx = idx + 1;
    if state.packet_idx < state.packet_len {
        return false;
    }
    state.packet_idx = 0;
    apply_packet(state)
}

fn apply_packet(state: &mut MouseState) -> bool {
    let header = state.packet[0];
    let new_buttons = header & 0x07;
    let overflow = (header & 0xC0) != 0;
    let old_buttons = state.buttons;
    state.buttons = new_buttons;
    if overflow {
        return new_buttons != old_buttons;
    }
    let dx = state.packet[1] as i8 as i32;
    let dy = state.packet[2] as i8 as i32;
    let mut new_x = state.x + dx;
    let mut new_y = state.y - dy;

    new_x = clamp_i32(new_x, 0, state.max_x);
    new_y = clamp_i32(new_y, 0, state.max_y);

    let changed = new_x != state.x || new_y != state.y || new_buttons != old_buttons;
    let mut changed = changed;
    state.x = new_x;
    state.y = new_y;
    if state.packet_len >= 4 {
        let mut dz = (state.packet[3] & 0x0F) as i8;
        if dz & 0x08 != 0 {
            dz |= !0x0F;
        }
        if dz != 0 {
            state.wheel_delta = state.wheel_delta.saturating_add(dz as i32);
            changed = true;
        }
    }
    changed
}

fn clamp_i32(value: i32, min: i32, max: i32) -> i32 {
    let mut v = value;
    let max = if max < min { min } else { max };
    if v < min {
        v = min;
    }
    if v > max {
        v = max;
    }
    v
}

fn read_status() -> u8 {
    unsafe { Port::<u8>::new(STATUS_PORT).read() }
}

fn read_data() -> u8 {
    unsafe { Port::<u8>::new(DATA_PORT).read() }
}

fn read_mouse_byte() -> Option<u8> {
    let status = read_status();
    if status & STATUS_OUT_FULL == 0 {
        return None;
    }
    if status & STATUS_AUX_DATA == 0 {
        return None;
    }
    Some(read_data())
}
