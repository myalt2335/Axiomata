use x86_64::instructions::port::Port;

const STATUS_PORT: u16 = 0x64;
const DATA_PORT: u16 = 0x60;

const CMD_READ_CTRL: u8 = 0x20;
const CMD_WRITE_CTRL: u8 = 0x60;
const CMD_DISABLE_PORT1: u8 = 0xAD;
const CMD_DISABLE_PORT2: u8 = 0xA7;
const CMD_ENABLE_PORT1: u8 = 0xAE;
const CMD_ENABLE_PORT2: u8 = 0xA8;
const CMD_WRITE_MOUSE: u8 = 0xD4;

const STATUS_OUT_FULL: u8 = 0x01;
const STATUS_IN_FULL: u8 = 0x02;

fn read_status() -> u8 {
    unsafe { Port::<u8>::new(STATUS_PORT).read() }
}

fn read_data() -> u8 {
    unsafe { Port::<u8>::new(DATA_PORT).read() }
}

fn write_command(cmd: u8) {
    let _ = wait_input_clear();
    unsafe { Port::<u8>::new(STATUS_PORT).write(cmd) };
}

fn write_data(data: u8) {
    let _ = wait_input_clear();
    unsafe { Port::<u8>::new(DATA_PORT).write(data) };
}

fn wait_input_clear() -> bool {
    for _ in 0..100_000 {
        if read_status() & STATUS_IN_FULL == 0 {
            return true;
        }
    }
    false
}

fn wait_output_full() -> bool {
    for _ in 0..100_000 {
        if read_status() & STATUS_OUT_FULL != 0 {
            return true;
        }
    }
    false
}

pub fn flush_output() {
    while read_status() & STATUS_OUT_FULL != 0 {
        let _ = read_data();
    }
}

pub fn read_output_byte() -> Option<u8> {
    if read_status() & STATUS_OUT_FULL == 0 {
        return None;
    }
    Some(read_data())
}

pub fn send_keyboard_command(cmd: u8) -> Option<u8> {
    write_data(cmd);
    if wait_output_full() {
        Some(read_data())
    } else {
        None
    }
}

pub fn send_mouse_command(cmd: u8) -> Option<u8> {
    write_command(CMD_WRITE_MOUSE);
    write_data(cmd);
    if wait_output_full() {
        Some(read_data())
    } else {
        None
    }
}

pub fn init_controller() -> bool {
    write_command(CMD_DISABLE_PORT1);
    write_command(CMD_DISABLE_PORT2);
    flush_output();

    write_command(CMD_READ_CTRL);
    if !wait_output_full() {
        return false;
    }
    let mut ctrl = read_data();
    ctrl |= 0x01 | 0x02 | 0x40;
    ctrl &= !0x30;

    write_command(CMD_WRITE_CTRL);
    write_data(ctrl);

    write_command(CMD_ENABLE_PORT1);
    write_command(CMD_ENABLE_PORT2);
    flush_output();
    true
}
