use uart_16550::SerialPort;
use spin::Mutex;
use lazy_static::lazy_static;

lazy_static! {
    static ref SERIAL1: Mutex<SerialPort> = {
        let mut serial_port = unsafe { SerialPort::new(0x3F8) };
        serial_port.init();
        Mutex::new(serial_port)
    };
}

pub fn write(msg: &str) {
    let mut serial = SERIAL1.lock();
    for byte in msg.bytes() {
        serial.send(byte);
    }
    serial.send(b'\r');
    serial.send(b'\n');
}

pub fn write_try(msg: &str) {
    let Some(mut serial) = SERIAL1.try_lock() else {
        return;
    };
    for byte in msg.bytes() {
        serial.send(byte);
    }
    serial.send(b'\r');
    serial.send(b'\n');
}
