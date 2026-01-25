use alloc::{format, string::String, string::ToString, vec, vec::Vec};
use core::cmp;

use crate::{
    block::{BlockDevice, BlockDeviceError},
    time,
};

const SECTOR_SIZE: usize = 512;
const DIR_ENTRY_SIZE: usize = 32;
const ENTRIES_PER_SECTOR: usize = SECTOR_SIZE / DIR_ENTRY_SIZE;
const FAT32_PART_TYPES: [u8; 2] = [0x0B, 0x0C];
const FAT_EOC: u32 = 0x0FFFFFF8;
const FAT_BAD: u32 = 0x0FFFFFF7;

#[derive(Copy, Clone)]
pub struct PartitionInfo {
    pub type_code: u8,
    pub lba_start: u32,
    pub sectors: u32,
}

#[derive(Copy, Clone)]
pub struct MbrInfo {
    pub signature: bool,
    pub is_empty: bool,
    pub partitions: [Option<PartitionInfo>; 4],
}

#[derive(Copy, Clone)]
pub struct Fat32Info {
    pub bytes_per_sector: u16,
    pub sectors_per_cluster: u8,
    #[allow(dead_code)]
    pub reserved_sectors: u16,
    #[allow(dead_code)]
    pub num_fats: u8,
    pub sectors_per_fat: u32,
    #[allow(dead_code)]
    pub root_cluster: u32,
    #[allow(dead_code)]
    pub total_sectors: u32,
    pub volume_label: [u8; 11],
}

#[derive(Copy, Clone)]
pub struct FatUsage {
    pub total_clusters: u32,
    pub free_clusters: u32,
    pub cluster_size: u32,
}

pub fn read_mbr<D: BlockDevice>(dev: &D) -> Result<MbrInfo, D::Error> {
    let mut sector = [0u8; SECTOR_SIZE];
    dev.read_block(0, &mut sector)?;

    let is_empty = sector.iter().all(|b| *b == 0);
    let signature = sector[510] == 0x55 && sector[511] == 0xAA;

    let mut partitions = [None; 4];
    if signature {
        for idx in 0..4 {
            let base = 446 + idx * 16;
            let type_code = sector[base + 4];
            let lba_start = u32::from_le_bytes([
                sector[base + 8],
                sector[base + 9],
                sector[base + 10],
                sector[base + 11],
            ]);
            let sectors = u32::from_le_bytes([
                sector[base + 12],
                sector[base + 13],
                sector[base + 14],
                sector[base + 15],
            ]);
            if type_code != 0 && sectors != 0 {
                partitions[idx] = Some(PartitionInfo {
                    type_code,
                    lba_start,
                    sectors,
                });
            }
        }
    }

    Ok(MbrInfo {
        signature,
        is_empty,
        partitions,
    })
}

pub fn find_fat32_partition(mbr: &MbrInfo) -> Option<PartitionInfo> {
    for part in mbr.partitions.iter().flatten() {
        if FAT32_PART_TYPES.contains(&part.type_code) {
            return Some(*part);
        }
    }
    None
}

fn read_sector<D: BlockDevice>(dev: &D, lba: u32, buf: &mut [u8; SECTOR_SIZE]) -> Result<(), &'static str> {
    dev.read_block(lba as u64, buf).map_err(|err| err.as_str())
}

fn write_sector<D: BlockDevice>(dev: &D, lba: u32, buf: &[u8; SECTOR_SIZE]) -> Result<(), &'static str> {
    dev.write_block(lba as u64, buf).map_err(|err| err.as_str())
}

fn is_eoc(value: u32) -> bool {
    value >= FAT_EOC
}

fn is_bad_cluster(value: u32) -> bool {
    value == FAT_BAD
}

fn fat_date_time() -> (u16, u16, u8) {
    let Some(secs) = time::current_time_secs() else {
        return (0, 0, 0);
    };
    let (year, month, day, hour, minute, second) = secs_to_ymd_hms(secs);
    let year = if year < 1980 { 1980 } else { year };
    let date = ((year - 1980) as u16) << 9
        | ((month as u16) << 5)
        | (day as u16);
    let time = ((hour as u16) << 11)
        | ((minute as u16) << 5)
        | ((second as u16) / 2);
    (date, time, 0)
}

fn is_short_name_char(ch: char) -> bool {
    if ch.is_ascii_alphanumeric() {
        return true;
    }
    matches!(
        ch,
        '!' | '#' | '$' | '%' | '&' | '\'' | '(' | ')' | '-' | '@' | '^' | '_' | '`' | '{' | '}' | '~'
    )
}

fn utf16_len(name: &str) -> usize {
    name.encode_utf16().count()
}

fn to_utf16(name: &str) -> Vec<u16> {
    name.encode_utf16().collect()
}

fn lfn_checksum(short_name: &[u8; 11]) -> u8 {
    let mut sum = 0u8;
    for b in short_name.iter() {
        sum = ((sum & 1) << 7).wrapping_add(sum >> 1).wrapping_add(*b);
    }
    sum
}

fn decode_short_name(name: &[u8; 11], nt_res: u8) -> String {
    let base = &name[0..8];
    let ext = &name[8..11];
    let mut base_str = String::new();
    let mut ext_str = String::new();

    for &b in base.iter() {
        if b == b' ' {
            break;
        }
        base_str.push(b as char);
    }
    for &b in ext.iter() {
        if b == b' ' {
            break;
        }
        ext_str.push(b as char);
    }

    if nt_res & 0x08 != 0 {
        base_str = base_str.to_ascii_lowercase();
    }
    if nt_res & 0x10 != 0 {
        ext_str = ext_str.to_ascii_lowercase();
    }

    if ext_str.is_empty() {
        base_str
    } else {
        format!("{}.{}", base_str, ext_str)
    }
}

fn needs_lfn(name: &str, base: &str, ext: &str) -> bool {
    if name.len() > 12 {
        return true;
    }
    if base.len() > 8 || ext.len() > 3 {
        return true;
    }
    if base.is_empty() {
        return true;
    }
    if base.chars().any(|c| c == ' ') || ext.chars().any(|c| c == ' ') {
        return true;
    }
    if base.chars().any(|c| !is_short_name_char(c)) {
        return true;
    }
    if ext.chars().any(|c| !is_short_name_char(c)) {
        return true;
    }
    let base_has_lower = base.chars().any(|c| c.is_ascii_lowercase());
    let base_has_upper = base.chars().any(|c| c.is_ascii_uppercase());
    let ext_has_lower = ext.chars().any(|c| c.is_ascii_lowercase());
    let ext_has_upper = ext.chars().any(|c| c.is_ascii_uppercase());
    if base_has_lower && base_has_upper {
        return true;
    }
    if ext_has_lower && ext_has_upper {
        return true;
    }
    false
}

fn build_short_name(name: &str) -> Result<([u8; 11], u8, bool), &'static str> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("Invalid file name.");
    }
    if utf16_len(trimmed) > 255 {
        return Err("File name is too long.");
    }
    let mut parts = trimmed.rsplitn(2, '.');
    let ext = parts.next().unwrap_or("");
    let base = parts.next().unwrap_or(trimmed);
    let ext = if base == trimmed { "" } else { ext };

    let need_lfn = needs_lfn(trimmed, base, ext);

    let base_upper = base.to_ascii_uppercase();
    let ext_upper = ext.to_ascii_uppercase();

    let mut nt_res = 0u8;
    if base.chars().all(|c| !c.is_ascii_uppercase()) && base.chars().any(|c| c.is_ascii_lowercase()) {
        nt_res |= 0x08;
    }
    if ext.chars().all(|c| !c.is_ascii_uppercase()) && ext.chars().any(|c| c.is_ascii_lowercase()) {
        nt_res |= 0x10;
    }

    let mut short = [b' '; 11];
    for (idx, ch) in base_upper.chars().take(8).enumerate() {
        if !need_lfn && !is_short_name_char(ch) {
            return Err("Invalid file name.");
        }
        if is_short_name_char(ch) {
            short[idx] = ch as u8;
        }
    }
    for (idx, ch) in ext_upper.chars().take(3).enumerate() {
        if !need_lfn && !is_short_name_char(ch) {
            return Err("Invalid file name.");
        }
        if is_short_name_char(ch) {
            short[8 + idx] = ch as u8;
        }
    }

    Ok((short, nt_res, need_lfn))
}

fn sanitize_short_component(component: &str) -> String {
    let mut out = String::new();
    for ch in component.chars() {
        if is_short_name_char(ch) {
            out.push(ch.to_ascii_uppercase());
        }
    }
    out
}

fn generate_short_alias(name: &str, existing: &[ [u8; 11] ]) -> Result<[u8; 11], &'static str> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err("Invalid file name.");
    }
    let mut parts = trimmed.rsplitn(2, '.');
    let ext = parts.next().unwrap_or("");
    let base = parts.next().unwrap_or(trimmed);
    let ext = if base == trimmed { "" } else { ext };

    let base_clean = sanitize_short_component(base);
    let ext_clean = sanitize_short_component(ext);
    let base_clean = if base_clean.is_empty() { "FILE".to_string() } else { base_clean };

    for suffix in 1..100u8 {
        let mut short = [b' '; 11];
        let mut base_part = base_clean.clone();
        if base_part.len() > 6 {
            base_part.truncate(6);
        }
        let tilde = format!("~{}", suffix);
        let mut final_base = base_part;
        final_base.push_str(&tilde);

        for (idx, ch) in final_base.chars().take(8).enumerate() {
            short[idx] = ch as u8;
        }
        for (idx, ch) in ext_clean.chars().take(3).enumerate() {
            short[8 + idx] = ch as u8;
        }

        if !existing.iter().any(|s| s == &short) {
            return Ok(short);
        }
    }

    Err("Unable to generate short name.")
}

fn encode_lfn_entries(name: &str, short_name: &[u8; 11]) -> Result<Vec<[u8; 32]>, &'static str> {
    let utf16 = to_utf16(name);
    if utf16.len() > 255 {
        return Err("File name is too long.");
    }
    let checksum = lfn_checksum(short_name);
    let mut entries = Vec::new();

    let total_parts = (utf16.len() + 12) / 13;
    for part in 0..total_parts {
        let seq = (total_parts - part) as u8;
        let is_last = part == 0;
        let mut entry = [0u8; 32];
        entry[11] = 0x0F;
        entry[13] = checksum;

        let seq_val = if is_last { seq | 0x40 } else { seq };
        entry[0] = seq_val;

        let mut chars = [0xFFFFu16; 13];
        let start = part * 13;
        let end = cmp::min(start + 13, utf16.len());
        for (idx, code) in utf16[start..end].iter().enumerate() {
            chars[idx] = *code;
        }
        if end < start + 13 {
            chars[end - start] = 0x0000;
        }

        for i in 0..5 {
            let offs = 1 + i * 2;
            entry[offs..offs + 2].copy_from_slice(&chars[i].to_le_bytes());
        }
        for i in 0..6 {
            let offs = 14 + i * 2;
            entry[offs..offs + 2].copy_from_slice(&chars[5 + i].to_le_bytes());
        }
        for i in 0..2 {
            let offs = 28 + i * 2;
            entry[offs..offs + 2].copy_from_slice(&chars[11 + i].to_le_bytes());
        }

        entries.push(entry);
    }

    Ok(entries)
}

fn decode_lfn_name(units: &[u16]) -> String {
    let mut name = String::new();
    let iter = units
        .iter()
        .take_while(|&&u| u != 0x0000 && u != 0xFFFF)
        .cloned();
    for ch in core::char::decode_utf16(iter) {
        match ch {
            Ok(ch) => name.push(ch),
            Err(_) => name.push('\u{FFFD}'),
        }
    }
    name
}

#[derive(Clone)]
pub struct DirEntryInfo {
    pub name: String,
    pub is_dir: bool,
    #[allow(dead_code)]
    pub attr: u8,
    pub cluster: u32,
    pub size: u32,
    pub entry_index: u32,
    pub lfn_entries: u8,
    pub short_name: [u8; 11],
    #[allow(dead_code)]
    pub nt_reserved: u8,
}

struct FatCache {
    lba: Option<u32>,
    buf: [u8; SECTOR_SIZE],
    dirty: bool,
}

impl FatCache {
    fn new() -> Self {
        Self {
            lba: None,
            buf: [0u8; SECTOR_SIZE],
            dirty: false,
        }
    }
}

pub struct Fat32Volume<D: BlockDevice> {
    device: D,
    pub part_start: u32,
    #[allow(dead_code)]
    pub part_sectors: u32,
    bytes_per_sector: u16,
    sectors_per_cluster: u8,
    reserved_sectors: u16,
    num_fats: u8,
    sectors_per_fat: u32,
    root_cluster: u32,
    fsinfo_sector: u16,
    total_sectors: u32,
    volume_label: [u8; 11],
    fat_cache: FatCache,
    free_count: Option<u32>,
    next_free: u32,
}

impl<D: BlockDevice> Fat32Volume<D> {
    pub fn info(&self) -> Fat32Info {
        Fat32Info {
            bytes_per_sector: self.bytes_per_sector,
            sectors_per_cluster: self.sectors_per_cluster,
            reserved_sectors: self.reserved_sectors,
            num_fats: self.num_fats,
            sectors_per_fat: self.sectors_per_fat,
            root_cluster: self.root_cluster,
            total_sectors: self.total_sectors,
            volume_label: self.volume_label,
        }
    }

    pub fn root_cluster(&self) -> u32 {
        self.root_cluster
    }

    pub fn usage(&mut self) -> Result<FatUsage, &'static str> {
        let total_clusters = self.cluster_count();
        let cluster_size = self.sectors_per_cluster as u32 * SECTOR_SIZE as u32;
        let free_clusters = self.free_count.ok_or("Free space unavailable.")?;
        Ok(FatUsage {
            total_clusters,
            free_clusters,
            cluster_size,
        })
    }

    pub fn open(dev: D, part: PartitionInfo) -> Result<Self, &'static str> {
        let mut sector = [0u8; SECTOR_SIZE];
        read_sector(&dev, part.lba_start, &mut sector)?;
        if sector[510] != 0x55 || sector[511] != 0xAA {
            return Err("Invalid FAT32 boot sector.");
        }

        let bytes_per_sector = u16::from_le_bytes([sector[11], sector[12]]);
        if bytes_per_sector != SECTOR_SIZE as u16 {
            return Err("Unsupported FAT32 sector size.");
        }
        let sectors_per_cluster = sector[13];
        if sectors_per_cluster == 0 {
            return Err("Invalid FAT32 cluster size.");
        }
        let reserved_sectors = u16::from_le_bytes([sector[14], sector[15]]);
        let num_fats = sector[16];
        let root_entries = u16::from_le_bytes([sector[17], sector[18]]);
        if root_entries != 0 {
            return Err("Not a FAT32 volume.");
        }
        let total_sectors_16 = u16::from_le_bytes([sector[19], sector[20]]);
        let total_sectors_32 = u32::from_le_bytes([sector[32], sector[33], sector[34], sector[35]]);
        let total_sectors = if total_sectors_16 != 0 {
            total_sectors_16 as u32
        } else {
            total_sectors_32
        };
        let sectors_per_fat = u32::from_le_bytes([sector[36], sector[37], sector[38], sector[39]]);
        let root_cluster = u32::from_le_bytes([sector[44], sector[45], sector[46], sector[47]]);
        let fsinfo_sector = u16::from_le_bytes([sector[48], sector[49]]);
        let mut volume_label = [b' '; 11];
        volume_label.copy_from_slice(&sector[71..82]);

        if total_sectors == 0 || sectors_per_fat == 0 {
            return Err("Invalid FAT32 size fields.");
        }

        let mut volume = Self {
            device: dev,
            part_start: part.lba_start,
            part_sectors: part.sectors,
            bytes_per_sector,
            sectors_per_cluster,
            reserved_sectors,
            num_fats,
            sectors_per_fat,
            root_cluster,
            fsinfo_sector,
            total_sectors,
            volume_label,
            fat_cache: FatCache::new(),
            free_count: None,
            next_free: 2,
        };

        volume.load_fsinfo().ok();
        Ok(volume)
    }

    pub fn format(dev: D, part_start: u32, part_sectors: u32, label: &str) -> Result<Self, &'static str> {
        if part_sectors < 8192 {
            return Err("Disk too small for FAT32.");
        }

        let bytes_per_sector = SECTOR_SIZE as u16;
        let sectors_per_cluster = choose_sectors_per_cluster(part_sectors);
        let reserved_sectors = 32u16;
        let num_fats = 2u8;

        let total_sectors = part_sectors;
        let mut sectors_per_fat = 1u32;
        loop {
            let data_sectors = total_sectors
                .saturating_sub(reserved_sectors as u32)
                .saturating_sub(num_fats as u32 * sectors_per_fat);
            let clusters = data_sectors / sectors_per_cluster as u32;
            let fat_bytes = clusters.saturating_mul(4);
            let needed = (fat_bytes + (bytes_per_sector as u32 - 1)) / bytes_per_sector as u32;
            if needed == sectors_per_fat {
                break;
            }
            sectors_per_fat = needed;
        }

        let root_cluster = 2u32;
        let fsinfo_sector = 1u16;
        let backup_boot = 6u16;
        let volume_label = build_volume_label(label);

        write_mbr(&dev, part_start, part_sectors)?;

        let mut boot = [0u8; SECTOR_SIZE];
        boot[0] = 0xEB;
        boot[1] = 0x58;
        boot[2] = 0x90;
        boot[3..11].copy_from_slice(b"AXIOMATA");
        boot[11..13].copy_from_slice(&bytes_per_sector.to_le_bytes());
        boot[13] = sectors_per_cluster;
        boot[14..16].copy_from_slice(&reserved_sectors.to_le_bytes());
        boot[16] = num_fats;
        boot[17..19].copy_from_slice(&0u16.to_le_bytes());
        boot[19..21].copy_from_slice(&0u16.to_le_bytes());
        boot[21] = 0xF8;
        boot[22..24].copy_from_slice(&0u16.to_le_bytes());
        boot[24..26].copy_from_slice(&63u16.to_le_bytes());
        boot[26..28].copy_from_slice(&255u16.to_le_bytes());
        boot[28..32].copy_from_slice(&part_start.to_le_bytes());
        boot[32..36].copy_from_slice(&total_sectors.to_le_bytes());
        boot[36..40].copy_from_slice(&sectors_per_fat.to_le_bytes());
        boot[40..42].copy_from_slice(&0u16.to_le_bytes());
        boot[42..44].copy_from_slice(&0u16.to_le_bytes());
        boot[44..48].copy_from_slice(&root_cluster.to_le_bytes());
        boot[48..50].copy_from_slice(&fsinfo_sector.to_le_bytes());
        boot[50..52].copy_from_slice(&backup_boot.to_le_bytes());
        boot[64] = 0x80;
        boot[66] = 0x29;
        let serial = volume_id_from_time();
        boot[67..71].copy_from_slice(&serial.to_le_bytes());
        boot[71..82].copy_from_slice(&volume_label);
        boot[82..90].copy_from_slice(b"FAT32   ");
        boot[510] = 0x55;
        boot[511] = 0xAA;

        write_sector(&dev, part_start, &boot)?;

        let fsinfo = build_fsinfo(total_sectors, reserved_sectors as u32, sectors_per_cluster as u32, num_fats as u32, sectors_per_fat);
        write_sector(&dev, part_start + fsinfo_sector as u32, &fsinfo)?;
        write_sector(&dev, part_start + backup_boot as u32, &boot)?;

        let zero = [0u8; SECTOR_SIZE];
        for offset in 2..reserved_sectors {
            if offset == backup_boot {
                continue;
            }
            if offset == fsinfo_sector {
                continue;
            }
            write_sector(&dev, part_start + offset as u32, &zero)?;
        }

        let fat_start = part_start + reserved_sectors as u32;
        let fat_total = num_fats as u32 * sectors_per_fat;
        for i in 0..fat_total {
            write_sector(&dev, fat_start + i, &zero)?;
        }

        let mut fat_sector = [0u8; SECTOR_SIZE];
        fat_sector[0..4].copy_from_slice(&0x0FFFFFF8u32.to_le_bytes());
        fat_sector[4..8].copy_from_slice(&0x0FFFFFFFu32.to_le_bytes());
        fat_sector[8..12].copy_from_slice(&0x0FFFFFFFu32.to_le_bytes());
        write_sector(&dev, fat_start, &fat_sector)?;
        write_sector(&dev, fat_start + sectors_per_fat, &fat_sector)?;

        let data_start = fat_start + fat_total;
        let cluster_buf = vec![0u8; SECTOR_SIZE * sectors_per_cluster as usize];
        for i in 0..sectors_per_cluster as u32 {
            let mut sector_buf = [0u8; SECTOR_SIZE];
            sector_buf.copy_from_slice(&cluster_buf[i as usize * SECTOR_SIZE..(i as usize + 1) * SECTOR_SIZE]);
            write_sector(&dev, data_start + i, &sector_buf)?;
        }

        let part = PartitionInfo {
            type_code: 0x0C,
            lba_start: part_start,
            sectors: part_sectors,
        };
        Self::open(dev, part)
    }

    pub fn read_directory(&mut self, cluster: u32) -> Result<Vec<DirEntryInfo>, &'static str> {
        let mut entries = Vec::new();
        let mut lfn_parts: Vec<(u8, Vec<u16>, u8)> = Vec::new();
        let mut lfn_count = 0u8;

        let mut current = if cluster == 0 { self.root_cluster } else { cluster };
        let mut entry_index = 0u32;

        loop {
            let next = self.read_fat_entry(current)?;
            if is_bad_cluster(next) {
                return Err("Bad cluster encountered.");
            }
            for sector_index in 0..self.sectors_per_cluster {
                let lba = self.cluster_to_lba(current) + sector_index as u32;
                let mut sector = [0u8; SECTOR_SIZE];
                read_sector(&self.device, lba, &mut sector)?;
                for slot in 0..ENTRIES_PER_SECTOR {
                    let offset = slot * DIR_ENTRY_SIZE;
                    let first = sector[offset];
                    if first == 0x00 {
                        return Ok(entries);
                    }
                    if first == 0xE5 {
                        lfn_parts.clear();
                        lfn_count = 0;
                        entry_index += 1;
                        continue;
                    }

                    let attr = sector[offset + 11];
                    if attr == 0x0F {
                        if let Some(part) = decode_lfn_entry(&sector[offset..offset + 32]) {
                            lfn_parts.push(part);
                            lfn_count = lfn_count.saturating_add(1);
                        }
                        entry_index += 1;
                        continue;
                    }

                    if attr & 0x08 != 0 {
                        lfn_parts.clear();
                        lfn_count = 0;
                        entry_index += 1;
                        continue;
                    }

                    let name = if !lfn_parts.is_empty() {
                        let short = &sector[offset..offset + 11];
                        let mut short_name = [0u8; 11];
                        short_name.copy_from_slice(short);
                        let checksum = lfn_checksum(&short_name);
                        let mut collected: Vec<(u8, Vec<u16>)> = Vec::new();
                        for (seq, chars, sum) in lfn_parts.drain(..) {
                            if sum == checksum {
                                collected.push((seq, chars));
                            }
                        }
                        collected.sort_by(|a, b| a.0.cmp(&b.0));
                        let mut utf16: Vec<u16> = Vec::new();
                        for (_, chunk) in collected {
                            utf16.extend(chunk);
                        }
                        let name = decode_lfn_name(&utf16);
                        if name.is_empty() {
                            let mut short_name = [0u8; 11];
                            short_name.copy_from_slice(&sector[offset..offset + 11]);
                            decode_short_name(&short_name, sector[offset + 12])
                        } else {
                            name
                        }
                    } else {
                        let mut short_name = [0u8; 11];
                        short_name.copy_from_slice(&sector[offset..offset + 11]);
                        decode_short_name(&short_name, sector[offset + 12])
                    };

                    let mut short_name = [0u8; 11];
                    short_name.copy_from_slice(&sector[offset..offset + 11]);
                    let nt_reserved = sector[offset + 12];

                    let cluster_lo = u16::from_le_bytes([sector[offset + 26], sector[offset + 27]]);
                    let cluster_hi = u16::from_le_bytes([sector[offset + 20], sector[offset + 21]]);
                    let cluster = ((cluster_hi as u32) << 16) | cluster_lo as u32;
                    let size = u32::from_le_bytes([
                        sector[offset + 28],
                        sector[offset + 29],
                        sector[offset + 30],
                        sector[offset + 31],
                    ]);

                    let is_dir = attr & 0x10 != 0;

                    entries.push(DirEntryInfo {
                        name,
                        is_dir,
                        attr,
                        cluster,
                        size,
                        entry_index,
                        lfn_entries: lfn_count,
                        short_name,
                        nt_reserved,
                    });

                    lfn_parts.clear();
                    lfn_count = 0;
                    entry_index += 1;
                }
            }

            if is_eoc(next) {
                break;
            }
            current = next;
        }

        Ok(entries)
    }

    pub fn find_entry(&mut self, dir_cluster: u32, name: &str) -> Result<Option<DirEntryInfo>, &'static str> {
        let entries = self.read_directory(dir_cluster)?;
        for entry in entries {
            if entry.name.eq_ignore_ascii_case(name) {
                return Ok(Some(entry));
            }
        }
        Ok(None)
    }

    pub fn create_entry(&mut self, dir_cluster: u32, name: &str, is_dir: bool) -> Result<DirEntryInfo, &'static str> {
        let trimmed = name.trim();
        if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
            return Err("Invalid file name.");
        }
        if utf16_len(trimmed) > 255 {
            return Err("File name is too long.");
        }
        if trimmed.chars().any(|c| c.is_control() || c == '/' || c == '\\') {
            return Err("Path segment may not contain control characters or slashes or backslashes.");
        }
        if self.find_entry(dir_cluster, trimmed)?.is_some() {
            return Err("File already exists.");
        }

        let (mut short_name, mut nt_reserved, need_lfn) = build_short_name(trimmed)?;
        let entries = self.read_directory(dir_cluster)?;
        let mut existing_short: Vec<[u8; 11]> = entries.iter().map(|e| e.short_name).collect();

        if need_lfn {
            short_name = generate_short_alias(trimmed, &existing_short)?;
            nt_reserved = 0;
        }

        existing_short.push(short_name);

        let lfn_entries = if need_lfn {
            encode_lfn_entries(trimmed, &short_name)?
        } else {
            Vec::new()
        };
        let total_needed = lfn_entries.len() + 1;
        let start_index = self.find_free_dir_slots(dir_cluster, total_needed as u32)?;

        let attr = if is_dir { 0x10 } else { 0x20 };
        let (date, time, tenth) = fat_date_time();

        let mut short_entry = [0u8; 32];
        short_entry[0..11].copy_from_slice(&short_name);
        short_entry[11] = attr;
        short_entry[12] = nt_reserved;
        short_entry[13] = tenth;
        short_entry[14..16].copy_from_slice(&time.to_le_bytes());
        short_entry[16..18].copy_from_slice(&date.to_le_bytes());
        short_entry[18..20].copy_from_slice(&date.to_le_bytes());
        short_entry[22..24].copy_from_slice(&time.to_le_bytes());
        short_entry[24..26].copy_from_slice(&date.to_le_bytes());

        let mut entry_cluster = 0u32;
        if is_dir {
            entry_cluster = self.allocate_cluster()?;
            let cluster_hi = (entry_cluster >> 16) as u16;
            let cluster_lo = (entry_cluster & 0xFFFF) as u16;
            short_entry[20..22].copy_from_slice(&cluster_hi.to_le_bytes());
            short_entry[26..28].copy_from_slice(&cluster_lo.to_le_bytes());
            self.init_directory_cluster(entry_cluster, dir_cluster)?;
        }

        let mut entries_bytes = lfn_entries;
        entries_bytes.push(short_entry);
        self.write_dir_entries(dir_cluster, start_index, &entries_bytes)?;
        self.flush_fat_cache()?;

        Ok(DirEntryInfo {
            name: trimmed.to_string(),
            is_dir,
            attr,
            cluster: entry_cluster,
            size: 0,
            entry_index: start_index + (entries_bytes.len() as u32 - 1),
            lfn_entries: (entries_bytes.len() as u8).saturating_sub(1),
            short_name,
            nt_reserved,
        })
    }

    pub fn read_file(&mut self, entry: &DirEntryInfo) -> Result<Vec<u8>, &'static str> {
        if entry.size == 0 || entry.cluster == 0 {
            return Ok(Vec::new());
        }
        let mut data = Vec::new();
        data.resize(entry.size as usize, 0u8);
        let mut remaining = entry.size as usize;
        let mut offset = 0usize;
        let mut current = entry.cluster;

        while remaining > 0 {
            let next = self.read_fat_entry(current)?;
            if is_bad_cluster(next) {
                return Err("Bad cluster encountered.");
            }
            for sector_index in 0..self.sectors_per_cluster {
                let lba = self.cluster_to_lba(current) + sector_index as u32;
                let mut sector = [0u8; SECTOR_SIZE];
                read_sector(&self.device, lba, &mut sector)?;
                let copy_len = cmp::min(SECTOR_SIZE, remaining);
                data[offset..offset + copy_len].copy_from_slice(&sector[..copy_len]);
                remaining -= copy_len;
                offset += copy_len;
                if remaining == 0 {
                    break;
                }
            }
            if is_eoc(next) {
                break;
            }
            current = next;
        }

        Ok(data)
    }

    pub fn write_file(&mut self, dir_cluster: u32, entry: &DirEntryInfo, contents: &[u8]) -> Result<(), &'static str> {
        let mut entry = entry.clone();
        if contents.len() > u32::MAX as usize {
            return Err("File too large.");
        }
        if entry.cluster != 0 {
            self.free_cluster_chain(entry.cluster)?;
            entry.cluster = 0;
        }

        if !contents.is_empty() {
            let cluster_size = self.sectors_per_cluster as usize * SECTOR_SIZE;
            let needed = (contents.len() + cluster_size - 1) / cluster_size;
            let chain = self.allocate_cluster_chain(needed as u32)?;
            entry.cluster = chain[0];

            let mut remaining = contents.len();
            let mut offset = 0usize;
            for cluster in chain {
                for sector_index in 0..self.sectors_per_cluster {
                    let lba = self.cluster_to_lba(cluster) + sector_index as u32;
                    let mut sector = [0u8; SECTOR_SIZE];
                    let copy_len = cmp::min(SECTOR_SIZE, remaining);
                    if copy_len > 0 {
                        sector[..copy_len].copy_from_slice(&contents[offset..offset + copy_len]);
                        offset += copy_len;
                        remaining -= copy_len;
                    }
                    write_sector(&self.device, lba, &sector)?;
                }
                if remaining == 0 {
                    break;
                }
            }
        }

        entry.size = contents.len() as u32;
        self.update_entry(dir_cluster, &entry)?;
        self.flush_fat_cache()
    }

    pub fn delete_entry(&mut self, dir_cluster: u32, entry: &DirEntryInfo) -> Result<(), &'static str> {
        if entry.is_dir {
            let contents = self.read_directory(entry.cluster)?;
            for item in contents {
                if item.name != "." && item.name != ".." {
                    return Err("Directory not empty.");
                }
            }
        }

        if entry.cluster != 0 {
            self.free_cluster_chain(entry.cluster)?;
        }
        self.mark_entry_deleted(dir_cluster, entry)?;
        self.flush_fat_cache()
    }

    pub fn update_entry(&mut self, dir_cluster: u32, entry: &DirEntryInfo) -> Result<(), &'static str> {
        let (date, time, _tenth) = fat_date_time();
        let mut sector = [0u8; SECTOR_SIZE];
        let loc = self.entry_location(dir_cluster, entry.entry_index)?;
        read_sector(&self.device, loc.lba, &mut sector)?;
        let offset = loc.offset;
        sector[offset + 18..offset + 20].copy_from_slice(&date.to_le_bytes());
        sector[offset + 22..offset + 24].copy_from_slice(&time.to_le_bytes());
        sector[offset + 24..offset + 26].copy_from_slice(&date.to_le_bytes());
        let cluster_hi = ((entry.cluster >> 16) as u16).to_le_bytes();
        let cluster_lo = ((entry.cluster & 0xFFFF) as u16).to_le_bytes();
        sector[offset + 20..offset + 22].copy_from_slice(&cluster_hi);
        sector[offset + 26..offset + 28].copy_from_slice(&cluster_lo);
        sector[offset + 28..offset + 32].copy_from_slice(&entry.size.to_le_bytes());
        write_sector(&self.device, loc.lba, &sector)
    }

    pub fn update_access_date(&mut self, dir_cluster: u32, entry: &DirEntryInfo) -> Result<(), &'static str> {
        let (date, _time, _tenth) = fat_date_time();
        let mut sector = [0u8; SECTOR_SIZE];
        let loc = self.entry_location(dir_cluster, entry.entry_index)?;
        read_sector(&self.device, loc.lba, &mut sector)?;
        sector[loc.offset + 18..loc.offset + 20].copy_from_slice(&date.to_le_bytes());
        write_sector(&self.device, loc.lba, &sector)
    }

    pub fn mark_entry_deleted(&mut self, dir_cluster: u32, entry: &DirEntryInfo) -> Result<(), &'static str> {
        let start = entry.entry_index.saturating_sub(entry.lfn_entries as u32);
        let total = entry.lfn_entries as u32 + 1;
        for idx in 0..total {
            let loc = self.entry_location(dir_cluster, start + idx)?;
            let mut sector = [0u8; SECTOR_SIZE];
            read_sector(&self.device, loc.lba, &mut sector)?;
            sector[loc.offset] = 0xE5;
            write_sector(&self.device, loc.lba, &sector)?;
        }
        Ok(())
    }

    fn init_directory_cluster(&mut self, cluster: u32, parent: u32) -> Result<(), &'static str> {
        let mut buf = vec![0u8; self.sectors_per_cluster as usize * SECTOR_SIZE];

        let (date, time, tenth) = fat_date_time();
        let dot = build_dir_entry(b".          ", 0x10, cluster, 0, date, time, tenth);
        let dotdot_cluster = if cluster == self.root_cluster { self.root_cluster } else { parent };
        let dotdot = build_dir_entry(b"..         ", 0x10, dotdot_cluster, 0, date, time, tenth);

        buf[0..32].copy_from_slice(&dot);
        buf[32..64].copy_from_slice(&dotdot);

        for sector_index in 0..self.sectors_per_cluster {
            let lba = self.cluster_to_lba(cluster) + sector_index as u32;
            let start = sector_index as usize * SECTOR_SIZE;
            let mut sector = [0u8; SECTOR_SIZE];
            sector.copy_from_slice(&buf[start..start + SECTOR_SIZE]);
            write_sector(&self.device, lba, &sector)?;
        }

        Ok(())
    }

    fn read_fat_entry(&mut self, cluster: u32) -> Result<u32, &'static str> {
        let fat_offset = cluster * 4;
        let sector_idx = fat_offset as usize / SECTOR_SIZE;
        let offset = fat_offset as usize % SECTOR_SIZE;
        let lba = self.part_start + self.reserved_sectors as u32 + sector_idx as u32;

        if self.fat_cache.lba != Some(lba) {
            self.flush_fat_cache()?;
            read_sector(&self.device, lba, &mut self.fat_cache.buf)?;
            self.fat_cache.lba = Some(lba);
        }

        let val = u32::from_le_bytes([
            self.fat_cache.buf[offset],
            self.fat_cache.buf[offset + 1],
            self.fat_cache.buf[offset + 2],
            self.fat_cache.buf[offset + 3],
        ]) & 0x0FFF_FFFF;
        Ok(val)
    }

    fn write_fat_entry(&mut self, cluster: u32, value: u32) -> Result<(), &'static str> {
        let fat_offset = cluster * 4;
        let sector_idx = fat_offset as usize / SECTOR_SIZE;
        let offset = fat_offset as usize % SECTOR_SIZE;
        let lba = self.part_start + self.reserved_sectors as u32 + sector_idx as u32;

        if self.fat_cache.lba != Some(lba) {
            self.flush_fat_cache()?;
            read_sector(&self.device, lba, &mut self.fat_cache.buf)?;
            self.fat_cache.lba = Some(lba);
        }

        let bytes = (value & 0x0FFF_FFFF).to_le_bytes();
        self.fat_cache.buf[offset..offset + 4].copy_from_slice(&bytes);
        self.fat_cache.dirty = true;
        Ok(())
    }

    fn flush_fat_cache(&mut self) -> Result<(), &'static str> {
        if !self.fat_cache.dirty {
            return Ok(());
        }
        let Some(lba) = self.fat_cache.lba else {
            return Ok(());
        };

        for fat_index in 0..self.num_fats {
            let target = lba + (fat_index as u32 * self.sectors_per_fat);
            write_sector(&self.device, target, &self.fat_cache.buf)?;
        }
        self.fat_cache.dirty = false;
        Ok(())
    }

    fn cluster_to_lba(&self, cluster: u32) -> u32 {
        let first_data = self.part_start + self.reserved_sectors as u32 + self.num_fats as u32 * self.sectors_per_fat;
        first_data + (cluster - 2) * self.sectors_per_cluster as u32
    }

    fn allocate_cluster(&mut self) -> Result<u32, &'static str> {
        let start = if self.next_free < 2 { 2 } else { self.next_free };
        let max_clusters = self.cluster_count();

        let mut cluster = start;
        loop {
            if cluster >= max_clusters + 2 {
                cluster = 2;
            }
            let val = self.read_fat_entry(cluster)?;
            if val == 0 {
                self.write_fat_entry(cluster, 0x0FFFFFFF)?;
                self.next_free = cluster + 1;
                if let Some(count) = self.free_count.as_mut() {
                    *count = count.saturating_sub(1);
                }
                self.write_fsinfo().ok();
                return Ok(cluster);
            }
            cluster += 1;
            if cluster == start {
                break;
            }
        }

        Err("Disk full.")
    }

    fn allocate_cluster_chain(&mut self, count: u32) -> Result<Vec<u32>, &'static str> {
        let mut chain = Vec::new();
        for _ in 0..count {
            let cluster = self.allocate_cluster()?;
            if let Some(prev) = chain.last() {
                self.write_fat_entry(*prev, cluster)?;
            }
            chain.push(cluster);
        }
        if let Some(last) = chain.last() {
            self.write_fat_entry(*last, 0x0FFFFFFF)?;
        }
        Ok(chain)
    }

    fn free_cluster_chain(&mut self, start: u32) -> Result<(), &'static str> {
        let mut current = start;
        while current >= 2 {
            let next = self.read_fat_entry(current)?;
            if is_bad_cluster(next) {
                return Err("Bad cluster encountered.");
            }
            self.write_fat_entry(current, 0)?;
            if let Some(count) = self.free_count.as_mut() {
                *count = count.saturating_add(1);
            }
            if is_eoc(next) {
                break;
            }
            current = next;
        }
        self.write_fsinfo().ok();
        Ok(())
    }

    fn cluster_count(&self) -> u32 {
        let data_sectors = self.total_sectors
            .saturating_sub(self.reserved_sectors as u32)
            .saturating_sub(self.num_fats as u32 * self.sectors_per_fat);
        data_sectors / self.sectors_per_cluster as u32
    }

    #[allow(dead_code)]
    fn count_free_clusters(&mut self, total_clusters: u32) -> Result<u32, &'static str> {
        let mut free = 0u32;
        let max_cluster = 2u32.saturating_add(total_clusters);
        for cluster in 2..max_cluster {
            let val = self.read_fat_entry(cluster)?;
            if val == 0 {
                free = free.saturating_add(1);
            }
        }
        Ok(free)
    }

    fn find_free_dir_slots(&mut self, dir_cluster: u32, needed: u32) -> Result<u32, &'static str> {
        let mut current = if dir_cluster == 0 { self.root_cluster } else { dir_cluster };
        let mut entry_index = 0u32;
        let mut run_start = 0u32;
        let mut run_len = 0u32;

        loop {
            for sector_index in 0..self.sectors_per_cluster {
                let lba = self.cluster_to_lba(current) + sector_index as u32;
                let mut sector = [0u8; SECTOR_SIZE];
                read_sector(&self.device, lba, &mut sector)?;
                for slot in 0..ENTRIES_PER_SECTOR {
                    let offset = slot * DIR_ENTRY_SIZE;
                    let first = sector[offset];
                    if first == 0x00 || first == 0xE5 {
                        if run_len == 0 {
                            run_start = entry_index;
                        }
                        run_len += 1;
                        if run_len >= needed {
                            return Ok(run_start);
                        }
                    } else {
                        run_len = 0;
                    }
                    entry_index += 1;
                }
            }

            let next = self.read_fat_entry(current)?;
            if is_bad_cluster(next) {
                return Err("Bad cluster encountered.");
            }
            if is_eoc(next) {
                break;
            }
            current = next;
        }

        let new_cluster = self.allocate_cluster()?;
        self.write_fat_entry(current, new_cluster)?;
        self.write_fat_entry(new_cluster, 0x0FFFFFFF)?;
        self.zero_cluster(new_cluster)?;
        Ok(entry_index)
    }

    fn write_dir_entries(&mut self, dir_cluster: u32, start_index: u32, entries: &[[u8; 32]]) -> Result<(), &'static str> {
        for (idx, entry) in entries.iter().enumerate() {
            let loc = self.entry_location(dir_cluster, start_index + idx as u32)?;
            let mut sector = [0u8; SECTOR_SIZE];
            read_sector(&self.device, loc.lba, &mut sector)?;
            sector[loc.offset..loc.offset + 32].copy_from_slice(entry);
            write_sector(&self.device, loc.lba, &sector)?;
        }
        Ok(())
    }

    fn entry_location(&mut self, dir_cluster: u32, entry_index: u32) -> Result<EntryLocation, &'static str> {
        let entries_per_cluster = ENTRIES_PER_SECTOR as u32 * self.sectors_per_cluster as u32;
        let mut current = if dir_cluster == 0 { self.root_cluster } else { dir_cluster };
        let mut cluster_offset = entry_index / entries_per_cluster;
        let entry_in_cluster = entry_index % entries_per_cluster;

        while cluster_offset > 0 {
            let next = self.read_fat_entry(current)?;
            if is_bad_cluster(next) {
                return Err("Bad cluster encountered.");
            }
            if is_eoc(next) {
                return Err("Directory entry out of range.");
            }
            current = next;
            cluster_offset -= 1;
        }

        let sector_in_cluster = entry_in_cluster / ENTRIES_PER_SECTOR as u32;
        let entry_in_sector = entry_in_cluster % ENTRIES_PER_SECTOR as u32;
        let lba = self.cluster_to_lba(current) + sector_in_cluster;
        let offset = entry_in_sector as usize * DIR_ENTRY_SIZE;

        Ok(EntryLocation { lba, offset })
    }

    fn zero_cluster(&mut self, cluster: u32) -> Result<(), &'static str> {
        let zero = [0u8; SECTOR_SIZE];
        for sector_index in 0..self.sectors_per_cluster {
            let lba = self.cluster_to_lba(cluster) + sector_index as u32;
            write_sector(&self.device, lba, &zero)?;
        }
        Ok(())
    }

    fn load_fsinfo(&mut self) -> Result<(), &'static str> {
        let mut sector = [0u8; SECTOR_SIZE];
        let lba = self.part_start + self.fsinfo_sector as u32;
        read_sector(&self.device, lba, &mut sector)?;
        let lead = u32::from_le_bytes([sector[0], sector[1], sector[2], sector[3]]);
        let sig = u32::from_le_bytes([sector[484], sector[485], sector[486], sector[487]]);
        if lead != 0x41615252 || sig != 0x61417272 {
            return Err("Invalid FSInfo.");
        }
        let free_count = u32::from_le_bytes([sector[488], sector[489], sector[490], sector[491]]);
        let next_free = u32::from_le_bytes([sector[492], sector[493], sector[494], sector[495]]);
        if free_count != 0xFFFF_FFFF {
            self.free_count = Some(free_count);
        }
        if next_free != 0xFFFF_FFFF && next_free >= 2 {
            self.next_free = next_free;
        }
        Ok(())
    }

    fn write_fsinfo(&mut self) -> Result<(), &'static str> {
        let mut sector = [0u8; SECTOR_SIZE];
        let lba = self.part_start + self.fsinfo_sector as u32;
        read_sector(&self.device, lba, &mut sector)?;
        if self.free_count.is_none() {
            return Ok(());
        }
        sector[0..4].copy_from_slice(&0x41615252u32.to_le_bytes());
        sector[484..488].copy_from_slice(&0x61417272u32.to_le_bytes());
        let free = self.free_count.unwrap_or(0xFFFF_FFFF);
        sector[488..492].copy_from_slice(&free.to_le_bytes());
        sector[492..496].copy_from_slice(&self.next_free.to_le_bytes());
        sector[510] = 0x55;
        sector[511] = 0xAA;
        write_sector(&self.device, lba, &sector)
    }
}

struct EntryLocation {
    lba: u32,
    offset: usize,
}

fn decode_lfn_entry(bytes: &[u8]) -> Option<(u8, Vec<u16>, u8)> {
    if bytes.len() < 32 || bytes[11] != 0x0F {
        return None;
    }
    let seq = bytes[0] & 0x1F;
    let checksum = bytes[13];
    let mut chars = Vec::new();
    for i in 0..5 {
        let off = 1 + i * 2;
        let val = u16::from_le_bytes([bytes[off], bytes[off + 1]]);
        chars.push(val);
    }
    for i in 0..6 {
        let off = 14 + i * 2;
        let val = u16::from_le_bytes([bytes[off], bytes[off + 1]]);
        chars.push(val);
    }
    for i in 0..2 {
        let off = 28 + i * 2;
        let val = u16::from_le_bytes([bytes[off], bytes[off + 1]]);
        chars.push(val);
    }
    Some((seq, chars, checksum))
}

fn build_dir_entry(name: &[u8; 11], attr: u8, cluster: u32, size: u32, date: u16, time: u16, tenth: u8) -> [u8; 32] {
    let mut entry = [0u8; 32];
    entry[0..11].copy_from_slice(name);
    entry[11] = attr;
    entry[13] = tenth;
    entry[14..16].copy_from_slice(&time.to_le_bytes());
    entry[16..18].copy_from_slice(&date.to_le_bytes());
    entry[18..20].copy_from_slice(&date.to_le_bytes());
    let cluster_hi = ((cluster >> 16) as u16).to_le_bytes();
    let cluster_lo = ((cluster & 0xFFFF) as u16).to_le_bytes();
    entry[20..22].copy_from_slice(&cluster_hi);
    entry[22..24].copy_from_slice(&time.to_le_bytes());
    entry[24..26].copy_from_slice(&date.to_le_bytes());
    entry[26..28].copy_from_slice(&cluster_lo);
    entry[28..32].copy_from_slice(&size.to_le_bytes());
    entry
}

fn build_volume_label(label: &str) -> [u8; 11] {
    let mut out = [b' '; 11];
    let mut idx = 0usize;
    for ch in label.chars() {
        if idx >= 11 {
            break;
        }
        if ch.is_ascii_alphanumeric() || ch == ' ' {
            out[idx] = ch.to_ascii_uppercase() as u8;
            idx += 1;
        }
    }
    out
}

fn volume_id_from_time() -> u32 {
    let (date, time, _tenth) = fat_date_time();
    ((date as u32) << 16) | time as u32
}

fn build_fsinfo(total_sectors: u32, reserved: u32, spc: u32, fats: u32, fatsz: u32) -> [u8; 512] {
    let mut sector = [0u8; 512];
    sector[0..4].copy_from_slice(&0x41615252u32.to_le_bytes());
    sector[484..488].copy_from_slice(&0x61417272u32.to_le_bytes());

    let data_sectors = total_sectors
        .saturating_sub(reserved)
        .saturating_sub(fats * fatsz);
    let clusters = data_sectors / spc;
    let free = clusters.saturating_sub(1);
    sector[488..492].copy_from_slice(&free.to_le_bytes());
    sector[492..496].copy_from_slice(&3u32.to_le_bytes());
    sector[510] = 0x55;
    sector[511] = 0xAA;
    sector
}

fn write_mbr<D: BlockDevice>(dev: &D, part_start: u32, part_sectors: u32) -> Result<(), &'static str> {
    let mut sector = [0u8; SECTOR_SIZE];
    sector[440..444].copy_from_slice(&volume_id_from_time().to_le_bytes());
    let base = 446;
    sector[base + 4] = 0x0C;
    sector[base + 8..base + 12].copy_from_slice(&part_start.to_le_bytes());
    sector[base + 12..base + 16].copy_from_slice(&part_sectors.to_le_bytes());
    sector[510] = 0x55;
    sector[511] = 0xAA;
    write_sector(dev, 0, &sector)
}

fn choose_sectors_per_cluster(total_sectors: u32) -> u8 {
    let bytes = total_sectors as u64 * SECTOR_SIZE as u64;
    if bytes < 8 * 1024 * 1024 {
        1
    } else if bytes < 260 * 1024 * 1024 {
        4
    } else if bytes < 8 * 1024 * 1024 * 1024 {
        8
    } else if bytes < 16 * 1024 * 1024 * 1024 {
        16
    } else if bytes < 32 * 1024 * 1024 * 1024 {
        32
    } else {
        64
    }
}

fn secs_to_ymd_hms(mut secs: u64) -> (u16, u8, u8, u8, u8, u8) {
    let mut year = 1970u16;
    let mut days = secs / 86400;
    secs %= 86400;

    loop {
        let dy = days_in_year(year as u64);
        if days >= dy {
            days -= dy;
            year += 1;
        } else {
            break;
        }
    }

    let mut month = 1u8;
    loop {
        let dm = days_in_month(year as u64, month as u64);
        if days >= dm {
            days -= dm;
            month += 1;
        } else {
            break;
        }
    }

    let day = (days + 1) as u8;
    let hour = (secs / 3600) as u8;
    let minute = ((secs % 3600) / 60) as u8;
    let second = (secs % 60) as u8;

    (year, month, day, hour, minute, second)
}

fn days_in_year(year: u64) -> u64 {
    if (year % 4 == 0 && year % 100 != 0) || (year % 400 == 0) {
        366
    } else {
        365
    }
}

fn days_in_month(year: u64, month: u64) -> u64 {
    const MONTH_DAYS: [u64; 12] = [31,28,31,30,31,30,31,31,30,31,30,31];
    let mut days = MONTH_DAYS[(month - 1) as usize];
    if month == 2 && days_in_year(year) == 366 {
        days += 1;
    }
    days
}
