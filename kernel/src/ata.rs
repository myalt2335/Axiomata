use core::convert::TryInto;
use core::hint::spin_loop;
use lazy_static::lazy_static;
use spin::Mutex;
use x86_64::instructions::port::Port;
use crate::block::{BlockDevice, BlockDeviceError};
use crate::pci;

const ATA_PRIMARY_IO: u16 = 0x1F0;
const ATA_PRIMARY_CTRL: u16 = 0x3F6;
const ATA_SECONDARY_IO: u16 = 0x170;
const ATA_SECONDARY_CTRL: u16 = 0x376;

const STATUS_BSY: u8 = 0x80;
const STATUS_DRQ: u8 = 0x08;
const STATUS_ERR: u8 = 0x01;

const CMD_IDENTIFY: u8 = 0xEC;
const CMD_READ_SECTORS: u8 = 0x20;
const CMD_WRITE_SECTORS: u8 = 0x30;
const CMD_CACHE_FLUSH: u8 = 0xE7;

const MAX_POLL: usize = 100_000;
const SECTOR_SIZE: usize = 512;

#[derive(Copy, Clone)]
pub struct PciIdeInfo {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
    pub prog_if: u8,
    pub command: u16,
    pub bar0: u32,
    pub bar1: u32,
    pub bar2: u32,
    pub bar3: u32,
}

#[derive(Copy, Clone)]
pub struct AtaIoConfig {
    pub primary_cmd: u16,
    pub primary_ctrl: u16,
    pub secondary_cmd: u16,
    pub secondary_ctrl: u16,
    pub pci: Option<PciIdeInfo>,
}

impl AtaIoConfig {
    const fn legacy() -> Self {
        Self {
            primary_cmd: ATA_PRIMARY_IO,
            primary_ctrl: ATA_PRIMARY_CTRL,
            secondary_cmd: ATA_SECONDARY_IO,
            secondary_ctrl: ATA_SECONDARY_CTRL,
            pci: None,
        }
    }
}

lazy_static! {
    static ref ATA_IO: Mutex<AtaIoConfig> = Mutex::new(AtaIoConfig::legacy());
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DriveSelect {
    PrimaryMaster,
    PrimarySlave,
    SecondaryMaster,
    SecondarySlave,
}

#[derive(Clone, Copy, Debug)]
pub struct AtaDevice {
    pub drive: DriveSelect,
    pub sectors: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AtaError {
    NoDevice,
    Timeout,
    Error,
}

impl BlockDeviceError for AtaError {
    fn as_str(&self) -> &'static str {
        match self {
            AtaError::NoDevice => "ATA device not found.",
            AtaError::Timeout => "ATA timeout.",
            AtaError::Error => "ATA error.",
        }
    }
}

impl BlockDevice for AtaDevice {
    type Error = AtaError;

    fn read_block(&self, block: u64, buf: &mut [u8]) -> Result<(), Self::Error> {
        if block > u32::MAX as u64 {
            return Err(AtaError::Error);
        }
        if buf.len() != SECTOR_SIZE {
            return Err(AtaError::Error);
        }
        let buf: &mut [u8; SECTOR_SIZE] = buf.try_into().map_err(|_| AtaError::Error)?;
        read_sector(self.drive, block as u32, buf)
    }

    fn write_block(&self, block: u64, buf: &[u8]) -> Result<(), Self::Error> {
        if block > u32::MAX as u64 {
            return Err(AtaError::Error);
        }
        if buf.len() != SECTOR_SIZE {
            return Err(AtaError::Error);
        }
        let buf: &[u8; SECTOR_SIZE] = buf.try_into().map_err(|_| AtaError::Error)?;
        write_sector(self.drive, block as u32, buf)
    }
}

struct AtaPorts {
    data: Port<u16>,
    error: Port<u8>,
    sector_count: Port<u8>,
    lba0: Port<u8>,
    lba1: Port<u8>,
    lba2: Port<u8>,
    drive: Port<u8>,
    status_cmd: Port<u8>,
    control: Port<u8>,
}

impl AtaPorts {
    fn new(drive: DriveSelect) -> Self {
        let cfg = ATA_IO.lock();
        let (cmd_base, ctrl_port) = if is_secondary(drive) {
            (cfg.secondary_cmd, cfg.secondary_ctrl)
        } else {
            (cfg.primary_cmd, cfg.primary_ctrl)
        };
        Self {
            data: Port::new(cmd_base + 0),
            error: Port::new(cmd_base + 1),
            sector_count: Port::new(cmd_base + 2),
            lba0: Port::new(cmd_base + 3),
            lba1: Port::new(cmd_base + 4),
            lba2: Port::new(cmd_base + 5),
            drive: Port::new(cmd_base + 6),
            status_cmd: Port::new(cmd_base + 7),
            control: Port::new(ctrl_port),
        }
    }
}

pub fn io_config() -> AtaIoConfig {
    *ATA_IO.lock()
}

fn io_base_from_bar(bar: u32) -> Option<u16> {
    if bar & 0x1 == 0 {
        return None;
    }
    let base = (bar & 0xFFFC) as u16;
    if base <= 1 {
        None
    } else {
        Some(base)
    }
}

fn resolve_ports(bar_cmd: u32, bar_ctrl: u32, legacy_cmd: u16, legacy_ctrl: u16) -> (u16, u16) {
    let cmd = io_base_from_bar(bar_cmd).unwrap_or(legacy_cmd);
    let ctrl = match io_base_from_bar(bar_ctrl) {
        Some(base) => base.wrapping_add(2),
        None => legacy_ctrl,
    };
    (cmd, ctrl)
}

pub fn init() {
    let Some((bus, device, function)) = pci::find_ide_controller() else {
        return;
    };

    pci::enable_io_space(bus, device, function);
    let mut info = pci::read_ide_controller(bus, device, function);

    let mut prog_if = info.prog_if;
    let mut changed = false;
    if (prog_if & 0x02) != 0 && (prog_if & 0x01) != 0 {
        prog_if &= !0x01;
        changed = true;
    }
    if (prog_if & 0x08) != 0 && (prog_if & 0x04) != 0 {
        prog_if &= !0x04;
        changed = true;
    }
    if changed {
        pci::write_prog_if(bus, device, function, prog_if);
        info = pci::read_ide_controller(bus, device, function);
    }

    let (primary_cmd, primary_ctrl) =
        resolve_ports(info.bar0, info.bar1, ATA_PRIMARY_IO, ATA_PRIMARY_CTRL);
    let (secondary_cmd, secondary_ctrl) =
        resolve_ports(info.bar2, info.bar3, ATA_SECONDARY_IO, ATA_SECONDARY_CTRL);

    let pci_info = PciIdeInfo {
        bus,
        device,
        function,
        prog_if: info.prog_if,
        command: info.command,
        bar0: info.bar0,
        bar1: info.bar1,
        bar2: info.bar2,
        bar3: info.bar3,
    };

    let mut cfg = ATA_IO.lock();
    cfg.primary_cmd = primary_cmd;
    cfg.primary_ctrl = primary_ctrl;
    cfg.secondary_cmd = secondary_cmd;
    cfg.secondary_ctrl = secondary_ctrl;
    cfg.pci = Some(pci_info);
}

fn is_secondary(drive: DriveSelect) -> bool {
    matches!(drive, DriveSelect::SecondaryMaster | DriveSelect::SecondarySlave)
}

fn is_slave(drive: DriveSelect) -> bool {
    matches!(drive, DriveSelect::PrimarySlave | DriveSelect::SecondarySlave)
}

fn drive_head(drive: DriveSelect, lba: u32) -> u8 {
    let base = if is_slave(drive) { 0xF0 } else { 0xE0 };
    base | ((lba >> 24) & 0x0F) as u8
}

fn io_wait(control: &mut Port<u8>) {
    unsafe { control.read() };
    unsafe { control.read() };
    unsafe { control.read() };
    unsafe { control.read() };
}

fn reset_channel(ports: &mut AtaPorts) {
    unsafe { ports.control.write(0x04); }
    io_wait(&mut ports.control);
    unsafe { ports.control.write(0x00); }
    io_wait(&mut ports.control);
}

fn wait_not_busy(ports: &mut AtaPorts) -> Result<(), AtaError> {
    let mut seen = false;
    for _ in 0..MAX_POLL {
        let status: u8 = unsafe { ports.status_cmd.read() };
        if status != 0x00 && status != 0xFF {
            seen = true;
            if status & STATUS_BSY == 0 {
                return Ok(());
            }
        }
        spin_loop();
    }
    if seen { Err(AtaError::Timeout) } else { Err(AtaError::NoDevice) }
}

fn wait_drq(ports: &mut AtaPorts) -> Result<(), AtaError> {
    let mut seen = false;
    for _ in 0..MAX_POLL {
        let status: u8 = unsafe { ports.status_cmd.read() };
        if status != 0x00 && status != 0xFF {
            seen = true;
            if status & STATUS_ERR != 0 {
                let _ = unsafe { ports.error.read() };
                return Err(AtaError::Error);
            }
            if status & STATUS_DRQ != 0 {
                return Ok(());
            }
        }
        spin_loop();
    }
    if seen { Err(AtaError::Timeout) } else { Err(AtaError::NoDevice) }
}

pub fn identify(drive: DriveSelect) -> Result<AtaDevice, AtaError> {
    let mut ports = AtaPorts::new(drive);
    unsafe { ports.control.write(0); }
    reset_channel(&mut ports);
    unsafe { ports.drive.write(drive_head(drive, 0)); }
    io_wait(&mut ports.control);
    unsafe {
        ports.sector_count.write(0);
        ports.lba0.write(0);
        ports.lba1.write(0);
        ports.lba2.write(0);
    }
    unsafe { ports.status_cmd.write(CMD_IDENTIFY); }

    wait_not_busy(&mut ports)?;
    wait_drq(&mut ports)?;

    let mut words = [0u16; 256];
    for slot in words.iter_mut() {
        *slot = unsafe { ports.data.read() };
    }

    let lba28 = ((words[61] as u32) << 16) | (words[60] as u32);
    let mut sectors = lba28 as u64;
    if (words[83] & (1 << 10)) != 0 {
        let lba48 = (words[103] as u64) << 48
            | (words[102] as u64) << 32
            | (words[101] as u64) << 16
            | (words[100] as u64);
        if lba48 != 0 {
            sectors = lba48;
        }
    }

    if sectors == 0 {
        return Err(AtaError::Error);
    }

    Ok(AtaDevice { drive, sectors })
}

pub fn read_sector(drive: DriveSelect, lba: u32, buf: &mut [u8; 512]) -> Result<(), AtaError> {
    if lba > 0x0FFF_FFFF {
        return Err(AtaError::Error);
    }

    let mut ports = AtaPorts::new(drive);
    unsafe { ports.control.write(0); }
    unsafe { ports.drive.write(drive_head(drive, lba)); }
    io_wait(&mut ports.control);
    wait_not_busy(&mut ports)?;
    unsafe {
        ports.error.write(0);
        ports.sector_count.write(1);
        ports.lba0.write(lba as u8);
        ports.lba1.write((lba >> 8) as u8);
        ports.lba2.write((lba >> 16) as u8);
        ports.status_cmd.write(CMD_READ_SECTORS);
    }
    io_wait(&mut ports.control);
    wait_drq(&mut ports)?;

    for idx in 0..256 {
        let word: u16 = unsafe { ports.data.read() };
        let bytes = word.to_le_bytes();
        buf[idx * 2] = bytes[0];
        buf[idx * 2 + 1] = bytes[1];
    }
    Ok(())
}

pub fn write_sector(drive: DriveSelect, lba: u32, buf: &[u8; 512]) -> Result<(), AtaError> {
    if lba > 0x0FFF_FFFF {
        return Err(AtaError::Error);
    }

    let mut ports = AtaPorts::new(drive);
    unsafe { ports.control.write(0); }
    unsafe { ports.drive.write(drive_head(drive, lba)); }
    io_wait(&mut ports.control);
    wait_not_busy(&mut ports)?;
    unsafe {
        ports.error.write(0);
        ports.sector_count.write(1);
        ports.lba0.write(lba as u8);
        ports.lba1.write((lba >> 8) as u8);
        ports.lba2.write((lba >> 16) as u8);
        ports.status_cmd.write(CMD_WRITE_SECTORS);
    }
    io_wait(&mut ports.control);
    wait_drq(&mut ports)?;

    for idx in 0..256 {
        let lo = buf[idx * 2];
        let hi = buf[idx * 2 + 1];
        let word = u16::from_le_bytes([lo, hi]);
        unsafe { ports.data.write(word); }
    }

    wait_not_busy(&mut ports)?;
    unsafe { ports.status_cmd.write(CMD_CACHE_FLUSH); }
    let _ = wait_not_busy(&mut ports);
    Ok(())
}
