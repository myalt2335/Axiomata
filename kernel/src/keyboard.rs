use pc_keyboard::{
    layouts::Us104Key, DecodedKey, HandleControl, Keyboard as PcKeyboard, KeyCode,
    KeyEvent as PcKeyEvent, KeyState, ScancodeSet1,
};
use spin::Mutex;
use x86_64::instructions::port::Port;
use x86_64::instructions::interrupts;
use crate::mouse;
use crate::ps2;

const KBD_SET_DEFAULTS: u8 = 0xF6;
const KBD_ENABLE_SCANNING: u8 = 0xF4;
const KBD_SET_SCANCODE: u8 = 0xF0;
const KBD_SET_SCANCODE_SET2: u8 = 0x02;
const KBD_ACK: u8 = 0xFA;

const SCANCODE_QUEUE_LEN: usize = 128;

struct ScancodeQueue {
    buf: [u8; SCANCODE_QUEUE_LEN],
    head: usize,
    tail: usize,
    len: usize,
    dropped: bool,
}

impl ScancodeQueue {
    const fn new() -> Self {
        Self {
            buf: [0; SCANCODE_QUEUE_LEN],
            head: 0,
            tail: 0,
            len: 0,
            dropped: false,
        }
    }
}

static SCANCODE_QUEUE: Mutex<ScancodeQueue> = Mutex::new(ScancodeQueue::new());

pub fn push_scancode(sc: u8) {
    if sc == 0xFA || sc == 0xFE {
        return;
    }
    interrupts::without_interrupts(|| {
        let mut q = SCANCODE_QUEUE.lock();
        if q.len == SCANCODE_QUEUE_LEN {
            q.dropped = true;
            return;
        }
        let tail = q.tail;
        q.buf[tail] = sc;
        q.tail = (tail + 1) % SCANCODE_QUEUE_LEN;
        q.len += 1;
    });
}

fn pop_scancode() -> Option<u8> {
    interrupts::without_interrupts(|| {
        let mut q = SCANCODE_QUEUE.lock();
        if q.len == 0 {
            return None;
        }
        let sc = q.buf[q.head];
        q.head = (q.head + 1) % SCANCODE_QUEUE_LEN;
        q.len -= 1;
        Some(sc)
    })
}

pub enum KeyEvent {
    Char(char),
    Backspace,
    CtrlBackspace,
    Delete,
    Enter,
    Up,
    Down,
    Left,
    Right,
    CtrlLeft,
    CtrlRight,
    ShiftLeft,
    ShiftRight,
    CtrlShiftLeft,
    CtrlShiftRight,
    CtrlA,
    CtrlC,
    CtrlV,
    CtrlX,
    Tab,
    AltTab,
    Start,
}

pub struct KeyboardState {
    kb: PcKeyboard<Us104Key, ScancodeSet1>,
    data: Port<u8>,
    status: Port<u8>,
}

impl KeyboardState {
    fn new() -> Self {
        Self {
            kb: PcKeyboard::new(ScancodeSet1::new(), Us104Key, HandleControl::Ignore),
            data: Port::new(0x60),
            status: Port::new(0x64),
        }
    }

    fn read_scancode(&mut self) -> Option<u8> {
        if let Some(sc) = pop_scancode() {
            return Some(sc);
        }
        let status: u8 = unsafe { self.status.read() };
        if status & 1 == 0 {
            return None;
        }
        if status & 0x20 != 0 {
            let sc: u8 = unsafe { self.data.read() };
            mouse::push_byte(sc);
            return None;
        }
        let sc: u8 = unsafe { self.data.read() };
        if sc == 0xFA || sc == 0xFE {
            return None;
        }
        Some(sc)
    }
}

pub struct Keyboard {
    inner: KeyboardState,
    ctrl_down: bool,
    shift_down: bool,
    alt_down: bool,
}

impl Keyboard {
    pub fn new() -> Self { Self { inner: KeyboardState::new(), ctrl_down: false, shift_down: false, alt_down: false } }

    fn update_modifiers(&mut self, evt: &PcKeyEvent) {
        if matches!(evt.code, KeyCode::LControl | KeyCode::RControl) {
            self.ctrl_down = matches!(evt.state, KeyState::Down | KeyState::SingleShot);
        }
        if matches!(evt.code, KeyCode::LShift | KeyCode::RShift) {
            self.shift_down = matches!(evt.state, KeyState::Down | KeyState::SingleShot);
        }
        if matches!(evt.code, KeyCode::LAlt) {
            self.alt_down = matches!(evt.state, KeyState::Down | KeyState::SingleShot);
        }
    }

    fn translate_backspace(&self) -> KeyEvent {
        if self.ctrl_down { KeyEvent::CtrlBackspace } else { KeyEvent::Backspace }
    }

    pub fn poll_event(&mut self) -> Option<KeyEvent> {
        if let Some(sc) = self.inner.read_scancode() {
            if let Ok(Some(evt)) = self.inner.kb.add_byte(sc) {
                self.update_modifiers(&evt);
                if let Some(key) = self.inner.kb.process_keyevent(evt) {
                    match key {
                        DecodedKey::Unicode(c) => match c {
                            '\u{0001}' | '\u{0041}' if self.ctrl_down => Some(KeyEvent::CtrlA),
                            '\u{0003}' | '\u{0043}' if self.ctrl_down => Some(KeyEvent::CtrlC),
                            '\u{0016}' | '\u{0056}' if self.ctrl_down => Some(KeyEvent::CtrlV),
                            '\u{0018}' | '\u{0058}' if self.ctrl_down => Some(KeyEvent::CtrlX),
                            '\n' | '\r' => Some(KeyEvent::Enter),
                            '\x08' => Some(self.translate_backspace()),
                            '\u{7f}' => Some(KeyEvent::Delete),
                            
                            
                            _ if self.ctrl_down => {
                                match c {
                                    'a' | 'A' => Some(KeyEvent::CtrlA),
                                    'c' | 'C' => Some(KeyEvent::CtrlC),
                                    'v' | 'V' => Some(KeyEvent::CtrlV),
                                    'x' | 'X' => Some(KeyEvent::CtrlX),
                                    _ => Some(KeyEvent::Char(c)),
                                }
                            }

                            '\t' => {
                                if self.alt_down {
                                    Some(KeyEvent::AltTab)
                                } else {
                                    Some(KeyEvent::Tab)
                                }
                            }
                            _ => Some(KeyEvent::Char(c)),
                        },
                        DecodedKey::RawKey(k) => {
                            match k {
                                KeyCode::Return => Some(KeyEvent::Enter),
                                KeyCode::Backspace => Some(self.translate_backspace()),
                                KeyCode::Delete => Some(KeyEvent::Delete),
                                KeyCode::Tab => {
                                    if self.alt_down {
                                        Some(KeyEvent::AltTab)
                                    } else {
                                        Some(KeyEvent::Tab)
                                    }
                                }
                                KeyCode::LWin | KeyCode::RWin => Some(KeyEvent::Start),
                                KeyCode::ArrowUp => Some(KeyEvent::Up),
                                KeyCode::ArrowDown => Some(KeyEvent::Down),
                                KeyCode::ArrowLeft => {
                                    if self.ctrl_down && self.shift_down { Some(KeyEvent::CtrlShiftLeft) }
                                    else if self.ctrl_down { Some(KeyEvent::CtrlLeft) }
                                    else if self.shift_down { Some(KeyEvent::ShiftLeft) }
                                    else { Some(KeyEvent::Left) }
                                }
                                KeyCode::ArrowRight => {
                                    if self.ctrl_down && self.shift_down { Some(KeyEvent::CtrlShiftRight) }
                                    else if self.ctrl_down { Some(KeyEvent::CtrlRight) }
                                    else if self.shift_down { Some(KeyEvent::ShiftRight) }
                                    else { Some(KeyEvent::Right) }
                                }
                                _ => None,
                            }
                        }
                    }
                } else { None }
            } else { None }
        } else { None }
    }
}

pub fn init() -> bool {
    ps2::flush_output();
    let ok_defaults = ps2::send_keyboard_command(KBD_SET_DEFAULTS) == Some(KBD_ACK);
    let ok_set = ps2::send_keyboard_command(KBD_SET_SCANCODE) == Some(KBD_ACK)
        && ps2::send_keyboard_command(KBD_SET_SCANCODE_SET2) == Some(KBD_ACK);
    let ok_enable = ps2::send_keyboard_command(KBD_ENABLE_SCANNING) == Some(KBD_ACK);
    ps2::flush_output();
    ok_defaults && ok_set && ok_enable
}
