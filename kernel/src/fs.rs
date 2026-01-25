use alloc::{format, string::String, string::ToString, vec, vec::Vec};
use core::cmp;
use lazy_static::lazy_static;
use spin::Mutex;

use crate::{ata, ext2, fat32};

const ROOT_DIR: &str = "\\";
const SEP: char = '\\';
const SEP_STR: &str = "\\";
const ALT_SEP: char = '/';
type AtaFat32Volume = fat32::Fat32Volume<ata::AtaDevice>;
type AtaExt2Volume = ext2::Ext2Volume<ata::AtaDevice>;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FsKind {
    Fat32,
    Ext2,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FsPreference {
    Auto,
    Fat32,
    Ext2,
}

enum VfsVolume {
    Fat32(AtaFat32Volume),
    Ext2(AtaExt2Volume),
}

#[derive(Clone)]
enum VfsEntry {
    Fat32(fat32::DirEntryInfo),
    Ext2(ext2::DirEntryInfo),
}

impl VfsEntry {
    fn name(&self) -> &str {
        match self {
            VfsEntry::Fat32(entry) => &entry.name,
            VfsEntry::Ext2(entry) => &entry.name,
        }
    }

    fn is_dir(&self) -> bool {
        match self {
            VfsEntry::Fat32(entry) => entry.is_dir,
            VfsEntry::Ext2(entry) => entry.is_dir,
        }
    }

    fn size(&self) -> u64 {
        match self {
            VfsEntry::Fat32(entry) => entry.size as u64,
            VfsEntry::Ext2(entry) => entry.size,
        }
    }

    fn dir_id(&self) -> u32 {
        match self {
            VfsEntry::Fat32(entry) => entry.cluster,
            VfsEntry::Ext2(entry) => entry.inode,
        }
    }
}

impl VfsVolume {
    fn root_id(&self) -> u32 {
        match self {
            VfsVolume::Fat32(volume) => volume.root_cluster(),
            VfsVolume::Ext2(volume) => volume.root_inode(),
        }
    }

    fn read_directory(&mut self, dir: u32) -> Result<Vec<VfsEntry>, &'static str> {
        match self {
            VfsVolume::Fat32(volume) => volume
                .read_directory(dir)
                .map(|entries| entries.into_iter().map(VfsEntry::Fat32).collect()),
            VfsVolume::Ext2(volume) => volume
                .read_directory(dir)
                .map(|entries| entries.into_iter().map(VfsEntry::Ext2).collect()),
        }
    }

    fn find_entry(&mut self, dir: u32, name: &str) -> Result<Option<VfsEntry>, &'static str> {
        match self {
            VfsVolume::Fat32(volume) => Ok(volume
                .find_entry(dir, name)?
                .map(VfsEntry::Fat32)),
            VfsVolume::Ext2(volume) => Ok(volume
                .find_entry(dir, name)?
                .map(VfsEntry::Ext2)),
        }
    }

    fn create_entry(&mut self, dir: u32, name: &str, is_dir: bool) -> Result<VfsEntry, &'static str> {
        match self {
            VfsVolume::Fat32(volume) => volume
                .create_entry(dir, name, is_dir)
                .map(VfsEntry::Fat32),
            VfsVolume::Ext2(volume) => volume
                .create_entry(dir, name, is_dir)
                .map(VfsEntry::Ext2),
        }
    }

    fn read_file(&mut self, entry: &VfsEntry) -> Result<Vec<u8>, &'static str> {
        match (self, entry) {
            (VfsVolume::Fat32(volume), VfsEntry::Fat32(entry)) => volume.read_file(entry),
            (VfsVolume::Ext2(volume), VfsEntry::Ext2(entry)) => volume.read_file(entry),
            _ => Err("Filesystem entry mismatch."),
        }
    }

    fn write_file(&mut self, dir: u32, entry: &VfsEntry, contents: &[u8]) -> Result<(), &'static str> {
        match (self, entry) {
            (VfsVolume::Fat32(volume), VfsEntry::Fat32(entry)) => {
                volume.write_file(dir, entry, contents)
            }
            (VfsVolume::Ext2(volume), VfsEntry::Ext2(entry)) => {
                volume.write_file(dir, entry, contents)
            }
            _ => Err("Filesystem entry mismatch."),
        }
    }

    fn delete_file(&mut self, dir: u32, entry: &VfsEntry) -> Result<(), &'static str> {
        match (self, entry) {
            (VfsVolume::Fat32(volume), VfsEntry::Fat32(entry)) => volume.delete_entry(dir, entry),
            (VfsVolume::Ext2(volume), VfsEntry::Ext2(entry)) => volume.delete_entry(dir, entry),
            _ => Err("Filesystem entry mismatch."),
        }
    }

    fn delete_dir(&mut self, dir: u32, entry: &VfsEntry) -> Result<(), &'static str> {
        match (self, entry) {
            (VfsVolume::Fat32(volume), VfsEntry::Fat32(entry)) => volume.delete_entry(dir, entry),
            (VfsVolume::Ext2(volume), VfsEntry::Ext2(entry)) => volume.delete_dir(dir, entry),
            _ => Err("Filesystem entry mismatch."),
        }
    }

    fn update_access_date(&mut self, dir: u32, entry: &VfsEntry) -> Result<(), &'static str> {
        match (self, entry) {
            (VfsVolume::Fat32(volume), VfsEntry::Fat32(entry)) => {
                volume.update_access_date(dir, entry)
            }
            (VfsVolume::Ext2(volume), VfsEntry::Ext2(entry)) => volume.update_access_date(entry),
            _ => Err("Filesystem entry mismatch."),
        }
    }
}

#[derive(Copy, Clone)]
pub enum ProbeResult {
    NotTried,
    IdentifyError(ata::AtaError),
    ReadError(ata::AtaError),
    Identified { sectors: u64, mbr: fat32::MbrInfo },
}

struct PersistState {
    enabled: bool,
    drive: Option<ata::DriveSelect>,
    sectors: u64,
    last_error: Option<&'static str>,
    partition: Option<fat32::PartitionInfo>,
    fs_kind: Option<FsKind>,
    fat32_info: Option<fat32::Fat32Info>,
    ext2_info: Option<ext2::Ext2Info>,
    preferred_fs: FsPreference,
    primary_master_probe: ProbeResult,
    primary_slave_probe: ProbeResult,
    secondary_master_probe: ProbeResult,
    secondary_slave_probe: ProbeResult,
}

impl PersistState {
    fn new() -> Self {
        Self {
            enabled: false,
            drive: None,
            sectors: 0,
            last_error: None,
            partition: None,
            fs_kind: None,
            fat32_info: None,
            ext2_info: None,
            preferred_fs: FsPreference::Auto,
            primary_master_probe: ProbeResult::NotTried,
            primary_slave_probe: ProbeResult::NotTried,
            secondary_master_probe: ProbeResult::NotTried,
            secondary_slave_probe: ProbeResult::NotTried,
        }
    }
}

lazy_static! {
    static ref VOLUME: Mutex<Option<VfsVolume>> = Mutex::new(None);
    static ref CWD: Mutex<String> = Mutex::new(String::from(ROOT_DIR));
    static ref PERSIST: Mutex<PersistState> = Mutex::new(PersistState::new());
}

#[derive(Copy, Clone)]
pub struct PersistInfo {
    pub enabled: bool,
    pub drive: Option<ata::DriveSelect>,
    pub sectors: u64,
    pub last_error: Option<&'static str>,
    pub partition: Option<fat32::PartitionInfo>,
    pub fs_kind: Option<FsKind>,
    pub fat32_info: Option<fat32::Fat32Info>,
    pub ext2_info: Option<ext2::Ext2Info>,
    pub preferred_fs: FsPreference,
    pub primary_master_probe: ProbeResult,
    pub primary_slave_probe: ProbeResult,
    pub secondary_master_probe: ProbeResult,
    pub secondary_slave_probe: ProbeResult,
}

pub struct UsageInfo {
    pub total_bytes: u64,
    pub free_bytes: u64,
    pub used_bytes: u64,
    pub used_percent: u8,
}

pub fn persist_info() -> PersistInfo {
    let state = PERSIST.lock();
    PersistInfo {
        enabled: state.enabled,
        drive: state.drive,
        sectors: state.sectors,
        last_error: state.last_error,
        partition: state.partition,
        fs_kind: state.fs_kind,
        fat32_info: state.fat32_info,
        ext2_info: state.ext2_info,
        preferred_fs: state.preferred_fs,
        primary_master_probe: state.primary_master_probe,
        primary_slave_probe: state.primary_slave_probe,
        secondary_master_probe: state.secondary_master_probe,
        secondary_slave_probe: state.secondary_slave_probe,
    }
}

pub fn fs_preference() -> FsPreference {
    PERSIST.lock().preferred_fs
}

pub fn set_fs_preference(preference: FsPreference) {
    let mut state = PERSIST.lock();
    state.preferred_fs = preference;
}

pub fn usage_info() -> Result<UsageInfo, &'static str> {
    with_volume(|volume| {
        let (total_bytes, free_bytes) = match volume {
            VfsVolume::Fat32(volume) => {
                let usage = volume.usage()?;
                let total_bytes = usage.total_clusters as u64 * usage.cluster_size as u64;
                let free_bytes = usage.free_clusters as u64 * usage.cluster_size as u64;
                (total_bytes, free_bytes)
            }
            VfsVolume::Ext2(volume) => {
                let usage = volume.usage()?;
                let total_bytes = usage.total_blocks as u64 * usage.block_size as u64;
                let free_bytes = usage.free_blocks as u64 * usage.block_size as u64;
                (total_bytes, free_bytes)
            }
        };
        let used_bytes = total_bytes.saturating_sub(free_bytes);
        let used_percent = if total_bytes == 0 {
            0
        } else {
            let pct = (used_bytes.saturating_mul(100) / total_bytes) as u8;
            if pct > 100 { 100 } else { pct }
        };
        Ok(UsageInfo {
            total_bytes,
            free_bytes,
            used_bytes,
            used_percent,
        })
    })
}

fn with_volume<T>(mut action: impl FnMut(&mut VfsVolume) -> Result<T, &'static str>) -> Result<T, &'static str> {
    let mut guard = VOLUME.lock();
    let Some(volume) = guard.as_mut() else {
        return Err("Persistent filesystem not available.");
    };
    action(volume)
}

fn normalize_segment(segment: &str) -> Result<String, &'static str> {
    let trimmed = segment.trim();
    if trimmed.is_empty() {
        return Err("Path segment cannot be empty.");
    }
    if trimmed.encode_utf16().count() > 255 {
        return Err("Path segment is too long (max 255 characters).");
    }
    if trimmed
        .chars()
        .any(|c| c.is_control() || c == ALT_SEP || c == SEP)
    {
        return Err("Path segment may not contain control characters or slashes or backslashes.");
    }
    Ok(trimmed.to_string())
}

fn normalize_separators(path: &str) -> String {
    let mut out = String::new();
    for ch in path.chars() {
        if ch == ALT_SEP {
            out.push(SEP);
        } else {
            out.push(ch);
        }
    }
    out
}

fn canonical_components(path: &str) -> Vec<String> {
    if path == ROOT_DIR || path == "/" {
        return Vec::new();
    }
    let normalized = normalize_separators(path);
    normalized
        .trim_start_matches(SEP)
        .split(SEP)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

fn resolve_path(path: &str, cwd: &str) -> Result<Vec<String>, &'static str> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err("Path cannot be empty.");
    }

    let normalized = normalize_separators(trimmed);
    let absolute = normalized.starts_with(SEP_STR);
    let mut components = if absolute {
        Vec::new()
    } else {
        canonical_components(cwd)
    };

    for segment in normalized.split(SEP) {
        if segment.is_empty() {
            continue;
        }
        if segment == "." {
            continue;
        }
        if segment == ".." {
            if !components.is_empty() {
                components.pop();
            }
            continue;
        }
        let display = normalize_segment(segment)?;
        components.push(display);
    }

    Ok(components)
}

fn resolve_from_cwd(path: &str) -> Result<Vec<String>, &'static str> {
    let cwd = current_dir();
    resolve_path(path, &cwd)
}

fn resolve_parent(path: &str) -> Result<(Vec<String>, String), &'static str> {
    let mut components = resolve_from_cwd(path)?;
    let Some(name) = components.pop() else {
        return Err("Invalid file path.");
    };
    if name == "." || name == ".." {
        return Err("Invalid file path.");
    }
    Ok((components, name))
}

fn resolve_dir(volume: &mut VfsVolume, components: &[String]) -> Result<(u32, Vec<String>), &'static str> {
    let mut cluster = volume.root_id();
    let mut stack: Vec<u32> = vec![cluster];
    let mut display: Vec<String> = Vec::new();

    for comp in components {
        if comp == "." {
            continue;
        }
        if comp == ".." {
            if stack.len() > 1 {
                stack.pop();
                cluster = *stack.last().unwrap_or(&cluster);
                display.pop();
            }
            continue;
        }
        let Some(entry) = volume.find_entry(cluster, comp)? else {
            return Err("Directory not found.");
        };
        if !entry.is_dir() {
            return Err("Not a directory.");
        }
        cluster = entry.dir_id();
        stack.push(cluster);
        display.push(entry.name().to_string());
    }

    Ok((cluster, display))
}

pub fn init_persistent() {
    *VOLUME.lock() = None;
    let primary_master_probe = probe_drive(ata::DriveSelect::PrimaryMaster);
    let primary_slave_probe = probe_drive(ata::DriveSelect::PrimarySlave);
    let secondary_master_probe = probe_drive(ata::DriveSelect::SecondaryMaster);
    let secondary_slave_probe = probe_drive(ata::DriveSelect::SecondarySlave);

    {
        let mut state = PERSIST.lock();
        state.enabled = false;
        state.drive = None;
        state.sectors = 0;
        state.last_error = None;
        state.partition = None;
        state.fs_kind = None;
        state.fat32_info = None;
        state.ext2_info = None;
        state.primary_master_probe = primary_master_probe;
        state.primary_slave_probe = primary_slave_probe;
        state.secondary_master_probe = secondary_master_probe;
        state.secondary_slave_probe = secondary_slave_probe;
    }

    let order = [
        (ata::DriveSelect::PrimarySlave, primary_slave_probe),
        (ata::DriveSelect::SecondaryMaster, secondary_master_probe),
        (ata::DriveSelect::SecondarySlave, secondary_slave_probe),
        (ata::DriveSelect::PrimaryMaster, primary_master_probe),
    ];

    let mut candidates: Vec<(ata::DriveSelect, u64, fat32::MbrInfo, Option<fat32::PartitionInfo>, Option<fat32::PartitionInfo>)> = Vec::new();
    for (drive, probe) in order {
        if let ProbeResult::Identified { sectors, mbr } = probe {
            let fat_part = fat32::find_fat32_partition(&mbr);
            let ext_part = ext2::find_ext2_partition(&mbr);
            candidates.push((drive, sectors, mbr, fat_part, ext_part));
        }
    }

    let preferred = fs_preference();
    let mut selected: Option<(ata::DriveSelect, u64, fat32::MbrInfo, Option<fat32::PartitionInfo>, Option<FsKind>)> = None;
    match preferred {
        FsPreference::Fat32 => {
            for (drive, sectors, mbr, fat_part, _ext_part) in candidates.iter() {
                if let Some(part) = fat_part {
                    selected = Some((*drive, *sectors, *mbr, Some(*part), Some(FsKind::Fat32)));
                    break;
                }
            }
        }
        FsPreference::Ext2 => {
            for (drive, sectors, mbr, _fat_part, ext_part) in candidates.iter() {
                if let Some(part) = ext_part {
                    selected = Some((*drive, *sectors, *mbr, Some(*part), Some(FsKind::Ext2)));
                    break;
                }
            }
        }
        FsPreference::Auto => {
            for (drive, sectors, mbr, fat_part, _ext_part) in candidates.iter() {
                if let Some(part) = fat_part {
                    selected = Some((*drive, *sectors, *mbr, Some(*part), Some(FsKind::Fat32)));
                    break;
                }
            }
            if selected.is_none() {
                for (drive, sectors, mbr, _fat_part, ext_part) in candidates.iter() {
                    if let Some(part) = ext_part {
                        selected = Some((*drive, *sectors, *mbr, Some(*part), Some(FsKind::Ext2)));
                        break;
                    }
                }
            }
        }
    }

    if selected.is_none() {
        if let Some((drive, sectors, mbr, _fat_part, _ext_part)) = candidates.first().copied() {
            selected = Some((drive, sectors, mbr, None, None));
        }
    }

    let Some((drive, sectors, mbr, part, kind)) = selected else {
        let mut state = PERSIST.lock();
        state.last_error = Some("No suitable ATA disk found.");
        return;
    };

    let dev = ata::AtaDevice { drive, sectors };
    let mut state = PERSIST.lock();
    state.drive = Some(drive);
    state.sectors = sectors;
    state.partition = part;
    state.fs_kind = kind;
    state.fat32_info = None;
    state.ext2_info = None;

    drop(state);
    *CWD.lock() = ROOT_DIR.to_string();

    if let (Some(part), Some(kind)) = (part, kind) {
        let open_result = match kind {
            FsKind::Fat32 => fat32::Fat32Volume::open(dev, part).map(VfsVolume::Fat32),
            FsKind::Ext2 => ext2::Ext2Volume::open(dev, part).map(VfsVolume::Ext2),
        };
        match open_result {
            Ok(volume) => {
                let (fat32_info, ext2_info) = match &volume {
                    VfsVolume::Fat32(vol) => (Some(vol.info()), None),
                    VfsVolume::Ext2(vol) => (None, Some(vol.info())),
                };
                *VOLUME.lock() = Some(volume);
                let mut state = PERSIST.lock();
                state.enabled = true;
                state.fs_kind = Some(kind);
                state.fat32_info = fat32_info;
                state.ext2_info = ext2_info;
                state.last_error = None;
                return;
            }
            Err(err) => {
                let mut state = PERSIST.lock();
                state.enabled = false;
                state.fs_kind = None;
                state.last_error = Some(err);
                return;
            }
        }
    }

    let empty_error = match preferred {
        FsPreference::Fat32 => "Disk appears empty; format to create FAT32.",
        FsPreference::Ext2 => "Disk appears empty; format to create EXT2.",
        FsPreference::Auto => "Disk appears empty; format to create FAT32 or EXT2.",
    };
    let missing_error = match preferred {
        FsPreference::Fat32 => "No FAT32 partition found.",
        FsPreference::Ext2 => "No EXT2 partition found.",
        FsPreference::Auto => "No FAT32 or EXT2 partition found.",
    };
    let mut state = PERSIST.lock();
    state.enabled = false;
    state.last_error = if !mbr.signature {
        if mbr.is_empty {
            Some(empty_error)
        } else {
            Some("Disk contains unrecognized data.")
        }
    } else {
        Some(missing_error)
    };
}

fn probe_drive(drive: ata::DriveSelect) -> ProbeResult {
    let dev = match ata::identify(drive) {
        Ok(dev) => dev,
        Err(err) => return ProbeResult::IdentifyError(err),
    };

    match fat32::read_mbr(&dev) {
        Ok(mbr) => ProbeResult::Identified {
            sectors: dev.sectors,
            mbr,
        },
        Err(err) => ProbeResult::ReadError(err),
    }
}

pub fn format_disk(target: Option<FsKind>) -> Result<(), &'static str> {
    let (drive, sectors) = {
        let state = PERSIST.lock();
        let Some(drive) = state.drive else {
            return Err("No ATA disk selected.");
        };
        (drive, state.sectors)
    };

    let explicit = target.is_some();
    let preference = fs_preference();
    let target = match target {
        Some(kind) => kind,
        None => match preference {
            FsPreference::Ext2 => FsKind::Ext2,
            FsPreference::Fat32 | FsPreference::Auto => FsKind::Fat32,
        },
    };

    let usable = cmp::min(sectors, 0x0FFF_FFFFu64);
    let total = usable as u32;
    let part_start = 2048u32;
    if target == FsKind::Fat32 && total <= part_start + 8192 {
        return Err("Disk too small for FAT32.");
    }
    let part_sectors = total - part_start;

    let dev = ata::AtaDevice { drive, sectors };
    let volume = match target {
        FsKind::Fat32 => {
            let volume = fat32::Fat32Volume::format(dev, part_start, part_sectors, "AXIOMATA")?;
            VfsVolume::Fat32(volume)
        }
        FsKind::Ext2 => {
            let volume = ext2::Ext2Volume::format(dev, part_start, part_sectors, "AXIOMATA")?;
            VfsVolume::Ext2(volume)
        }
    };
    let (fat32_info, ext2_info) = match &volume {
        VfsVolume::Fat32(vol) => (Some(vol.info()), None),
        VfsVolume::Ext2(vol) => (None, Some(vol.info())),
    };

    *VOLUME.lock() = Some(volume);
    let mut state = PERSIST.lock();
    state.enabled = true;
    state.partition = Some(fat32::PartitionInfo {
        type_code: match target {
            FsKind::Fat32 => 0x0C,
            FsKind::Ext2 => ext2::EXT2_PART_TYPE,
        },
        lba_start: part_start,
        sectors: part_sectors,
    });
    state.fs_kind = Some(target);
    state.fat32_info = fat32_info;
    state.ext2_info = ext2_info;
    if explicit {
        state.preferred_fs = match target {
            FsKind::Fat32 => FsPreference::Fat32,
            FsKind::Ext2 => FsPreference::Ext2,
        };
    }
    state.last_error = None;
    let probe = probe_drive(drive);
    match drive {
        ata::DriveSelect::PrimaryMaster => state.primary_master_probe = probe,
        ata::DriveSelect::PrimarySlave => state.primary_slave_probe = probe,
        ata::DriveSelect::SecondaryMaster => state.secondary_master_probe = probe,
        ata::DriveSelect::SecondarySlave => state.secondary_slave_probe = probe,
    }
    *CWD.lock() = ROOT_DIR.to_string();
    Ok(())
}

fn list_dir_internal(volume: &mut VfsVolume, cluster: u32) -> Result<Vec<ListingEntry>, &'static str> {
    let mut entries = Vec::new();
    for entry in volume.read_directory(cluster)? {
        if entry.name() == "." || entry.name() == ".." {
            continue;
        }
        entries.push(ListingEntry {
            name: entry.name().to_string(),
            size: size_to_usize(entry.size()),
            is_dir: entry.is_dir(),
        });
    }
    Ok(entries)
}

fn size_to_usize(value: u64) -> usize {
    if value > usize::MAX as u64 {
        usize::MAX
    } else {
        value as usize
    }
}

pub fn current_dir() -> String {
    CWD.lock().clone()
}

pub fn display_cwd() -> String {
    let cwd = current_dir();
    if cwd == ROOT_DIR {
        ".\\".to_string()
    } else {
        let rel = cwd.trim_start_matches(SEP);
        format!(".\\{}", rel)
    }
}

pub fn prompt_path() -> String {
    let cwd = current_dir();
    if cwd == ROOT_DIR {
        ".\\".to_string()
    } else {
        let rel = cwd.trim_start_matches(SEP);
        format!(".\\{}\\", rel)
    }
}

pub fn set_current_dir(path: &str) -> Result<(), &'static str> {
    with_volume(|volume| {
        let components = resolve_from_cwd(path)?;
        let (cluster, display) = resolve_dir(volume, &components)?;
        let new_path = if display.is_empty() {
            ROOT_DIR.to_string()
        } else {
            format!("{}{}", ROOT_DIR, display.join("\\"))
        };
        *CWD.lock() = new_path;
        let _ = cluster;
        Ok(())
    })
}

#[allow(dead_code)]
pub fn canonical_name(name: &str) -> Result<String, &'static str> {
    let components = resolve_from_cwd(name)?;
    if components.is_empty() {
        return Ok(ROOT_DIR.to_string());
    }
    Ok(format!("{}{}", ROOT_DIR, components.join("\\")))
}

pub fn touch(name: &str) -> Result<(), &'static str> {
    with_volume(|volume| {
        let (parent_components, file_name) = resolve_parent(name)?;
        let (parent_cluster, _) = resolve_dir(volume, &parent_components)?;
        if let Some(entry) = volume.find_entry(parent_cluster, &file_name)? {
            if entry.is_dir() {
                return Err("A directory with that name already exists.");
            }
            return Err("File already exists.");
        }
        let entry = volume.create_entry(parent_cluster, &file_name, false)?;
        let _ = entry;
        Ok(())
    })
}

pub fn ensure_file(name: &str) -> Result<String, &'static str> {
    with_volume(|volume| {
        let (parent_components, file_name) = resolve_parent(name)?;
        let (parent_cluster, _) = resolve_dir(volume, &parent_components)?;
        if let Some(entry) = volume.find_entry(parent_cluster, &file_name)? {
            if entry.is_dir() {
                return Err("A directory with that name already exists.");
            }
            return Ok(entry.name().to_string());
        }
        let entry = volume.create_entry(parent_cluster, &file_name, false)?;
        Ok(entry.name().to_string())
    })
}

pub fn write_file(name: &str, contents: &str) -> Result<(), &'static str> {
    with_volume(|volume| {
        let (parent_components, file_name) = resolve_parent(name)?;
        let (parent_cluster, _) = resolve_dir(volume, &parent_components)?;
        let entry = if let Some(entry) = volume.find_entry(parent_cluster, &file_name)? {
            if entry.is_dir() {
                return Err("Not a file.");
            }
            entry
        } else {
            volume.create_entry(parent_cluster, &file_name, false)?
        };
        volume.write_file(parent_cluster, &entry, contents.as_bytes())
    })
}

pub fn append_line(name: &str, line: &str) -> Result<(), &'static str> {
    let mut body = read_file(name).unwrap_or_default();
    if !body.is_empty() {
        body.push('\n');
    }
    body.push_str(line);
    write_file(name, &body)
}

pub fn read_file(name: &str) -> Option<String> {
    with_volume(|volume| {
        let components = resolve_from_cwd(name)?;
        let (parent_components, file_name) = match components.split_last() {
            Some((name, parent)) => (parent.to_vec(), name.clone()),
            None => return Err("Invalid file path."),
        };
        let (parent_cluster, _) = resolve_dir(volume, &parent_components)?;
        let Some(entry) = volume.find_entry(parent_cluster, &file_name)? else {
            return Err("File not found.");
        };
        if entry.is_dir() {
            return Err("Not a file.");
        }
        let data = volume.read_file(&entry)?;
        let _ = volume.update_access_date(parent_cluster, &entry);
        String::from_utf8(data).map_err(|_| "Invalid file encoding.")
    }).ok()
}

pub fn delete_file(name: &str) -> Result<(), &'static str> {
    with_volume(|volume| {
        let (parent_components, file_name) = resolve_parent(name)?;
        let (parent_cluster, _) = resolve_dir(volume, &parent_components)?;
        let Some(entry) = volume.find_entry(parent_cluster, &file_name)? else {
            return Err("File not found.");
        };
        if entry.is_dir() {
            return Err("Not a file.");
        }
        volume.delete_file(parent_cluster, &entry)
    })
}

#[allow(dead_code)]
pub fn exists(name: &str) -> bool {
    with_volume(|volume| {
        let components = resolve_from_cwd(name)?;
        let (parent_components, file_name) = match components.split_last() {
            Some((name, parent)) => (parent.to_vec(), name.clone()),
            None => return Err("Invalid file path."),
        };
        let (parent_cluster, _) = resolve_dir(volume, &parent_components)?;
        Ok(volume.find_entry(parent_cluster, &file_name)?.is_some())
    })
    .unwrap_or(false)
}

#[derive(Clone)]
pub struct ListingEntry {
    pub name: String,
    pub size: usize,
    pub is_dir: bool,
}

pub fn list_files() -> Vec<ListingEntry> {
    with_volume(|volume| {
        let components = resolve_from_cwd(".")?;
        let (cluster, _) = resolve_dir(volume, &components)?;
        list_dir_internal(volume, cluster)
    })
    .unwrap_or_default()
}

pub fn list_dir(path: &str) -> Result<Vec<ListingEntry>, &'static str> {
    with_volume(|volume| {
        let components = resolve_from_cwd(path)?;
        let (cluster, _) = resolve_dir(volume, &components)?;
        list_dir_internal(volume, cluster)
    })
}

pub fn mkdir(path: &str) -> Result<(), &'static str> {
    with_volume(|volume| {
        let (parent_components, dir_name) = resolve_parent(path)?;
        let (parent_cluster, _) = resolve_dir(volume, &parent_components)?;
        if let Some(entry) = volume.find_entry(parent_cluster, &dir_name)? {
            if entry.is_dir() {
                return Err("Directory already exists.");
            }
            return Err("File already exists.");
        }
        volume.create_entry(parent_cluster, &dir_name, true)?;
        Ok(())
    })
}

pub fn rmdir(path: &str) -> Result<(), &'static str> {
    with_volume(|volume| {
        let trimmed = path.trim();
        if trimmed == ROOT_DIR || trimmed == "/" {
            return Err("Cannot remove root directory.");
        }
        let (parent_components, dir_name) = resolve_parent(path)?;
        let (parent_cluster, _) = resolve_dir(volume, &parent_components)?;
        let Some(entry) = volume.find_entry(parent_cluster, &dir_name)? else {
            return Err("Directory not found.");
        };
        if !entry.is_dir() {
            return Err("Not a directory.");
        }
        volume.delete_dir(parent_cluster, &entry)
    })
}
