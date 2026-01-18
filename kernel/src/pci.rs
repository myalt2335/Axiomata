use x86_64::instructions::port::Port;

const PCI_CONFIG_ADDR: u16 = 0xCF8;
const PCI_CONFIG_DATA: u16 = 0xCFC;

#[derive(Copy, Clone)]
pub struct IdeController {
    pub prog_if: u8,
    pub command: u16,
    pub bar0: u32,
    pub bar1: u32,
    pub bar2: u32,
    pub bar3: u32,
}

fn config_address(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    0x8000_0000
        | ((bus as u32) << 16)
        | ((device as u32) << 11)
        | ((function as u32) << 8)
        | (offset as u32 & 0xFC)
}

fn read_u32(bus: u8, device: u8, function: u8, offset: u8) -> u32 {
    let addr = config_address(bus, device, function, offset);
    unsafe {
        let mut cfg = Port::<u32>::new(PCI_CONFIG_ADDR);
        let mut data = Port::<u32>::new(PCI_CONFIG_DATA);
        cfg.write(addr);
        data.read()
    }
}

fn write_u32(bus: u8, device: u8, function: u8, offset: u8, value: u32) {
    let addr = config_address(bus, device, function, offset);
    unsafe {
        let mut cfg = Port::<u32>::new(PCI_CONFIG_ADDR);
        let mut data = Port::<u32>::new(PCI_CONFIG_DATA);
        cfg.write(addr);
        data.write(value);
    }
}

fn read_u16(bus: u8, device: u8, function: u8, offset: u8) -> u16 {
    let value = read_u32(bus, device, function, offset);
    let shift = (offset & 2) * 8;
    ((value >> shift) & 0xFFFF) as u16
}

fn read_u8(bus: u8, device: u8, function: u8, offset: u8) -> u8 {
    let value = read_u32(bus, device, function, offset);
    let shift = (offset & 3) * 8;
    ((value >> shift) & 0xFF) as u8
}

fn write_u8(bus: u8, device: u8, function: u8, offset: u8, value: u8) {
    let aligned = offset & 0xFC;
    let shift = (offset & 3) * 8;
    let mut data = read_u32(bus, device, function, aligned);
    data &= !(0xFF << shift);
    data |= (value as u32) << shift;
    write_u32(bus, device, function, aligned, data);
}

pub fn find_ide_controller() -> Option<(u8, u8, u8)> {
    for bus in 0u8..=255 {
        for device in 0u8..32 {
            for function in 0u8..8 {
                let vendor = read_u16(bus, device, function, 0x00);
                if vendor == 0xFFFF {
                    if function == 0 {
                        break;
                    }
                    continue;
                }
                let class = read_u8(bus, device, function, 0x0B);
                let subclass = read_u8(bus, device, function, 0x0A);
                if class == 0x01 && subclass == 0x01 {
                    return Some((bus, device, function));
                }
            }
        }
    }
    None
}

pub fn read_ide_controller(bus: u8, device: u8, function: u8) -> IdeController {
    IdeController {
        prog_if: read_u8(bus, device, function, 0x09),
        command: read_u16(bus, device, function, 0x04),
        bar0: read_u32(bus, device, function, 0x10),
        bar1: read_u32(bus, device, function, 0x14),
        bar2: read_u32(bus, device, function, 0x18),
        bar3: read_u32(bus, device, function, 0x1C),
    }
}

pub fn enable_io_space(bus: u8, device: u8, function: u8) {
    let mut cmd = read_u32(bus, device, function, 0x04);
    if cmd & 0x1 == 0 {
        cmd |= 0x1;
        write_u32(bus, device, function, 0x04, cmd);
    }
}

pub fn write_prog_if(bus: u8, device: u8, function: u8, prog_if: u8) {
    write_u8(bus, device, function, 0x09, prog_if);
}
