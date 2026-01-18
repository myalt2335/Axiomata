use alloc::{format, string::String, string::ToString, vec, vec::Vec};
use core::cmp;

use crate::block::{BlockDevice, BlockDeviceError};
use crate::console;
use crate::fat32::{MbrInfo, PartitionInfo};
use crate::time;

const SECTOR_SIZE: usize = 512;
const BLOCK_SIZE: usize = 4096;
const BLOCK_SECTORS: u32 = (BLOCK_SIZE / SECTOR_SIZE) as u32;
const EXT2_SUPER_MAGIC: u16 = 0xEF53;
const EXT2_REV_DYNAMIC: u32 = 1;
const EXT2_FIRST_INO: u32 = 11;
const INODE_SIZE: u16 = 128;
const ROOT_INODE: u32 = 2;
pub const EXT2_PART_TYPE: u8 = 0x83;

const EXT2_FT_REG_FILE: u8 = 1;
const EXT2_FT_DIR: u8 = 2;

struct FormatProgress {
    label: &'static str,
    total: u64,
    current: u64,
    last_percent: u8,
}

impl FormatProgress {
    fn new(label: &'static str, total: u64) -> Self {
        let progress = Self {
            label,
            total,
            current: 0,
            last_percent: 0,
        };
        progress.emit(0);
        progress
    }

    fn advance(&mut self, amount: u64) {
        if self.total == 0 {
            return;
        }
        self.current = self.current.saturating_add(amount);
        if self.current > self.total {
            self.current = self.total;
        }
        let percent = ((self.current.saturating_mul(100)) / self.total) as u8;
        if percent >= self.last_percent.saturating_add(5) || percent == 100 {
            self.last_percent = percent;
            self.emit(percent);
        }
    }

    fn finish(&mut self) {
        if self.last_percent < 100 {
            self.emit(100);
            self.last_percent = 100;
        }
        console::write_line("");
    }

    fn emit(&self, percent: u8) {
        let bar = progress_bar(percent, 20);
        console::write_inline(&format!("{} {} {}%", self.label, bar, percent));
    }
}

fn progress_bar(percent: u8, width: usize) -> String {
    let filled = (percent as usize * width) / 100;
    let mut out = String::from("[");
    for i in 0..width {
        if i < filled {
            out.push('#');
        } else {
            out.push('-');
        }
    }
    out.push(']');
    out
}

#[derive(Copy, Clone)]
pub struct Ext2Info {
    pub block_size: u32,
    pub blocks_count: u32,
    pub free_blocks_count: u32,
    pub inodes_count: u32,
    pub free_inodes_count: u32,
    pub blocks_per_group: u32,
    pub inodes_per_group: u32,
    pub groups_count: u32,
    pub inode_size: u16,
    pub first_data_block: u32,
    pub part_sectors: u32,
    pub volume_name: [u8; 16],
}

#[derive(Copy, Clone)]
pub struct Ext2Usage {
    pub total_blocks: u32,
    pub free_blocks: u32,
    pub block_size: u32,
}

pub fn find_ext2_partition(mbr: &MbrInfo) -> Option<PartitionInfo> {
    for part in mbr.partitions.iter().flatten() {
        if part.type_code == EXT2_PART_TYPE {
            return Some(*part);
        }
    }
    None
}

#[derive(Clone)]
pub struct DirEntryInfo {
    pub name: String,
    pub inode: u32,
    pub is_dir: bool,
    pub size: u64,
    pub block: u32,
    pub offset: u16,
    #[allow(dead_code)]
    pub rec_len: u16,
}

#[derive(Clone, Copy)]
struct Superblock {
    inodes_count: u32,
    blocks_count: u32,
    free_blocks_count: u32,
    free_inodes_count: u32,
    first_data_block: u32,
    log_block_size: u32,
    blocks_per_group: u32,
    inodes_per_group: u32,
    mtime: u32,
    wtime: u32,
    mnt_count: u16,
    max_mnt_count: u16,
    magic: u16,
    state: u16,
    errors: u16,
    rev_level: u32,
    first_ino: u32,
    inode_size: u16,
    volume_name: [u8; 16],
}

#[derive(Clone, Copy)]
struct GroupDesc {
    block_bitmap: u32,
    inode_bitmap: u32,
    inode_table: u32,
    free_blocks_count: u16,
    free_inodes_count: u16,
    used_dirs_count: u16,
}

#[derive(Clone, Copy)]
struct Inode {
    mode: u16,
    uid: u16,
    size: u32,
    atime: u32,
    ctime: u32,
    mtime: u32,
    dtime: u32,
    gid: u16,
    links_count: u16,
    blocks: u32,
    flags: u32,
    block: [u32; 15],
}

impl Inode {
    fn is_dir(&self) -> bool {
        self.mode & 0xF000 == 0x4000
    }

    fn is_file(&self) -> bool {
        self.mode & 0xF000 == 0x8000
    }
}

pub struct Ext2Volume<D: BlockDevice> {
    device: D,
    pub part_start: u32,
    #[allow(dead_code)]
    pub part_sectors: u32,
    block_size: u32,
    blocks_count: u32,
    inodes_count: u32,
    free_blocks_count: u32,
    free_inodes_count: u32,
    blocks_per_group: u32,
    inodes_per_group: u32,
    inode_size: u16,
    first_data_block: u32,
    groups_count: u32,
    group_desc: Vec<GroupDesc>,
    volume_name: [u8; 16],
}

impl<D: BlockDevice> Ext2Volume<D> {
    pub fn info(&self) -> Ext2Info {
        Ext2Info {
            block_size: self.block_size,
            blocks_count: self.blocks_count,
            free_blocks_count: self.free_blocks_count,
            inodes_count: self.inodes_count,
            free_inodes_count: self.free_inodes_count,
            blocks_per_group: self.blocks_per_group,
            inodes_per_group: self.inodes_per_group,
            groups_count: self.groups_count,
            inode_size: self.inode_size,
            first_data_block: self.first_data_block,
            part_sectors: self.part_sectors,
            volume_name: self.volume_name,
        }
    }

    pub fn usage(&mut self) -> Result<Ext2Usage, &'static str> {
        Ok(Ext2Usage {
            total_blocks: self.blocks_count,
            free_blocks: self.free_blocks_count,
            block_size: self.block_size,
        })
    }

    pub fn open(dev: D, part: PartitionInfo) -> Result<Self, &'static str> {
        let mut block0 = vec![0u8; BLOCK_SIZE];
        read_block_raw(&dev, part.lba_start as u32, 0, &mut block0)?;
        let sb = parse_superblock(&block0)?;

        if sb.magic != EXT2_SUPER_MAGIC {
            return Err("Invalid EXT2 superblock.");
        }
        let block_size = 1024u32 << sb.log_block_size;
        if block_size != BLOCK_SIZE as u32 {
            return Err("Unsupported EXT2 block size.");
        }
        if sb.rev_level < EXT2_REV_DYNAMIC {
            return Err("Unsupported EXT2 revision.");
        }

        let groups_count = (sb.blocks_count + sb.blocks_per_group - 1) / sb.blocks_per_group;
        let group_desc = read_group_descs(&dev, part.lba_start as u32, groups_count)?;

        Ok(Self {
            device: dev,
            part_start: part.lba_start as u32,
            part_sectors: part.sectors as u32,
            block_size,
            blocks_count: sb.blocks_count,
            inodes_count: sb.inodes_count,
            free_blocks_count: sb.free_blocks_count,
            free_inodes_count: sb.free_inodes_count,
            blocks_per_group: sb.blocks_per_group,
            inodes_per_group: sb.inodes_per_group,
            inode_size: sb.inode_size,
            first_data_block: sb.first_data_block,
            groups_count,
            group_desc,
            volume_name: sb.volume_name,
        })
    }

    pub fn format(dev: D, part_start: u32, part_sectors: u32, label: &str) -> Result<Self, &'static str> {
        let total_blocks = (part_sectors as u64 * SECTOR_SIZE as u64 / BLOCK_SIZE as u64) as u32;
        if total_blocks < 256 {
            return Err("Disk too small for EXT2.");
        }

        write_mbr(&dev, part_start, part_sectors)?;

        let blocks_per_group = cmp::min(32768u32, total_blocks);
        let inodes_per_group = compute_inodes_per_group(blocks_per_group);
        let groups_count = (total_blocks + blocks_per_group - 1) / blocks_per_group;
        let inode_table_blocks = inode_table_blocks(inodes_per_group);
        let group_desc_blocks = ((groups_count as u64 * 32 + (BLOCK_SIZE as u64 - 1))
            / BLOCK_SIZE as u64) as u64;
        let total_steps = 1
            + group_desc_blocks
            + (groups_count as u64 * (2 + inode_table_blocks as u64))
            + 2;
        let mut progress = FormatProgress::new("Formatting EXT2:", total_steps);

        let mut group_desc = Vec::new();
        let mut free_blocks_total = 0u32;
        let mut free_inodes_total = 0u32;

        for group in 0..groups_count {
            let group_start = group * blocks_per_group;
            let blocks_in_group = blocks_in_group(total_blocks, blocks_per_group, group);
            let (block_bitmap, inode_bitmap, inode_table) = if group == 0 {
                (2u32, 3u32, 4u32)
            } else {
                (group_start, group_start + 1, group_start + 2)
            };
            let reserved = if group == 0 {
                2 + 2 + inode_table_blocks
            } else {
                2 + inode_table_blocks
            };
            let mut free_blocks = blocks_in_group.saturating_sub(reserved);
            if group == 0 && free_blocks > 0 {
                free_blocks = free_blocks.saturating_sub(1);
            }

            let used_inodes = if group == 0 { 10 } else { 0 };
            let free_inodes = inodes_per_group.saturating_sub(used_inodes);

            group_desc.push(GroupDesc {
                block_bitmap,
                inode_bitmap,
                inode_table,
                free_blocks_count: free_blocks as u16,
                free_inodes_count: free_inodes as u16,
                used_dirs_count: if group == 0 { 1 } else { 0 },
            });

            free_blocks_total = free_blocks_total.saturating_add(free_blocks);
            free_inodes_total = free_inodes_total.saturating_add(free_inodes);
        }

        let sb = Superblock {
            inodes_count: inodes_per_group.saturating_mul(groups_count),
            blocks_count: total_blocks,
            free_blocks_count: free_blocks_total,
            free_inodes_count: free_inodes_total,
            first_data_block: 0,
            log_block_size: 2,
            blocks_per_group,
            inodes_per_group,
            mtime: 0,
            wtime: time::current_time_secs().unwrap_or(0) as u32,
            mnt_count: 0,
            max_mnt_count: 0xFFFF,
            magic: EXT2_SUPER_MAGIC,
            state: 1,
            errors: 1,
            rev_level: EXT2_REV_DYNAMIC,
            first_ino: EXT2_FIRST_INO,
            inode_size: INODE_SIZE,
            volume_name: build_volume_name(label),
        };

        write_superblock(&dev, part_start, &sb)?;
        progress.advance(1);
        write_group_descs(&dev, part_start, groups_count, &group_desc)?;
        progress.advance(group_desc_blocks);

        for group in 0..groups_count {
            let blocks_in_group = blocks_in_group(total_blocks, blocks_per_group, group);
            let reserved = if group == 0 {
                2 + 2 + inode_table_blocks
            } else {
                2 + inode_table_blocks
            };
            let mut bitmap = vec![0u8; BLOCK_SIZE];
            for bit in 0..reserved {
                set_bit(&mut bitmap, bit);
            }

            if group == 0 {
                let root_block = root_dir_block(inode_table_blocks);
                set_bit(&mut bitmap, root_block);
            }

            for bit in blocks_in_group..blocks_per_group {
                set_bit(&mut bitmap, bit);
            }

            let desc = group_desc[group as usize];
            write_block_raw(&dev, part_start, desc.block_bitmap, &bitmap)?;
            progress.advance(1);

            let mut inode_bitmap = vec![0u8; BLOCK_SIZE];
            if group == 0 {
                for inode_idx in 0..10u32 {
                    set_bit(&mut inode_bitmap, inode_idx);
                }
            }
            write_block_raw(&dev, part_start, desc.inode_bitmap, &inode_bitmap)?;
            progress.advance(1);

            let zero = vec![0u8; BLOCK_SIZE];
            for offset in 0..inode_table_blocks {
                write_block_raw(
                    &dev,
                    part_start,
                    desc.inode_table + offset,
                    &zero,
                )?;
                progress.advance(1);
            }
        }

        let part = PartitionInfo {
            type_code: EXT2_PART_TYPE,
            lba_start: part_start,
            sectors: part_sectors,
        };
        let mut volume = Self::open(dev, part)?;

        let root_block = root_dir_block(inode_table_blocks);
        volume.write_root_dir(root_block)?;
        progress.advance(1);

        let now = time::current_time_secs().unwrap_or(0) as u32;
        let root_inode = Inode {
            mode: 0x41ED,
            uid: 0,
            size: BLOCK_SIZE as u32,
            atime: now,
            ctime: now,
            mtime: now,
            dtime: 0,
            gid: 0,
            links_count: 2,
            blocks: BLOCK_SECTORS,
            flags: 0,
            block: {
                let mut b = [0u32; 15];
                b[0] = root_block;
                b
            },
        };
        volume.write_inode(ROOT_INODE, &root_inode)?;
        progress.advance(1);

        progress.finish();

        Ok(volume)
    }

    pub fn root_inode(&self) -> u32 {
        ROOT_INODE
    }

    pub fn read_directory(&mut self, inode: u32) -> Result<Vec<DirEntryInfo>, &'static str> {
        let dir_inode = self.read_inode(inode)?;
        if !dir_inode.is_dir() {
            return Err("Not a directory.");
        }

        let mut entries = Vec::new();
        let mut block_idx = 0u32;
        let mut remaining = dir_inode.size as u64;
        while remaining > 0 {
            let block = self.inode_block(&dir_inode, block_idx)?;
            if block == 0 {
                break;
            }
            let mut buf = vec![0u8; BLOCK_SIZE];
            self.read_block(block, &mut buf)?;
            let mut offset = 0usize;
            while offset + 8 <= BLOCK_SIZE {
                let inode_num = read_u32(&buf, offset);
                let rec_len = read_u16(&buf, offset + 4) as usize;
                let name_len = buf[offset + 6] as usize;
                let file_type = buf[offset + 7];

                if rec_len == 0 || offset + rec_len > BLOCK_SIZE {
                    return Err("Invalid EXT2 directory entry.");
                }

                if inode_num != 0 && name_len > 0 && offset + 8 + name_len <= BLOCK_SIZE {
                    let name_bytes = &buf[offset + 8..offset + 8 + name_len];
                    let name = core::str::from_utf8(name_bytes)
                        .map_err(|_| "Invalid EXT2 filename.")?
                        .to_string();
                    let entry_inode = self.read_inode(inode_num)?;
                    let is_dir = if file_type == 0 {
                        entry_inode.is_dir()
                    } else {
                        file_type == EXT2_FT_DIR
                    };

                    entries.push(DirEntryInfo {
                        name,
                        inode: inode_num,
                        is_dir,
                        size: entry_inode.size as u64,
                        block,
                        offset: offset as u16,
                        rec_len: rec_len as u16,
                    });
                }

                offset += rec_len;
                if offset >= BLOCK_SIZE {
                    break;
                }
            }

            block_idx += 1;
            remaining = remaining.saturating_sub(BLOCK_SIZE as u64);
        }

        Ok(entries)
    }

    pub fn find_entry(&mut self, dir_inode: u32, name: &str) -> Result<Option<DirEntryInfo>, &'static str> {
        let entries = self.read_directory(dir_inode)?;
        for entry in entries {
            if entry.name == name {
                return Ok(Some(entry));
            }
        }
        Ok(None)
    }

    pub fn create_entry(&mut self, dir_inode: u32, name: &str, is_dir: bool) -> Result<DirEntryInfo, &'static str> {
        let trimmed = name.trim();
        if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
            return Err("Invalid file name.");
        }
        if trimmed.len() > 255 {
            return Err("File name is too long.");
        }
        if trimmed.chars().any(|c| c.is_control() || c == '/' || c == '\\') {
            return Err("Path segment may not contain control characters or slashes or backslashes.");
        }
        if self.find_entry(dir_inode, trimmed)?.is_some() {
            return Err("File already exists.");
        }

        let inode_num = self.allocate_inode(is_dir)?;
        let now = time::current_time_secs().unwrap_or(0) as u32;
        let mut inode = Inode {
            mode: if is_dir { 0x41ED } else { 0x81A4 },
            uid: 0,
            size: 0,
            atime: now,
            ctime: now,
            mtime: now,
            dtime: 0,
            gid: 0,
            links_count: if is_dir { 2 } else { 1 },
            blocks: 0,
            flags: 0,
            block: [0u32; 15],
        };

        if is_dir {
            let data_block = self.allocate_block()?;
            inode.block[0] = data_block;
            inode.size = BLOCK_SIZE as u32;
            inode.blocks = BLOCK_SECTORS;
            self.write_directory_block(data_block, inode_num, dir_inode)?;
        }

        self.write_inode(inode_num, &inode)?;
        if let Err(err) = self.insert_dir_entry(
            dir_inode,
            trimmed,
            inode_num,
            if is_dir { EXT2_FT_DIR } else { EXT2_FT_REG_FILE },
        ) {
            let _ = self.free_inode_blocks(&inode);
            let _ = self.free_inode(inode_num);
            return Err(err);
        }

        if is_dir {
            let mut parent = self.read_inode(dir_inode)?;
            parent.links_count = parent.links_count.saturating_add(1);
            self.write_inode(dir_inode, &parent)?;
        }

        self.find_entry(dir_inode, trimmed)?.ok_or("Failed to create entry.")
    }

    pub fn read_file(&mut self, entry: &DirEntryInfo) -> Result<Vec<u8>, &'static str> {
        let inode = self.read_inode(entry.inode)?;
        if !inode.is_file() {
            return Err("Not a file.");
        }
        if inode.size == 0 {
            return Ok(Vec::new());
        }

        let mut data = vec![0u8; inode.size as usize];
        let mut remaining = inode.size as usize;
        let mut offset = 0usize;
        let mut block_idx = 0u32;

        while remaining > 0 {
            let block = self.inode_block(&inode, block_idx)?;
            if block == 0 {
                break;
            }
            let mut buf = vec![0u8; BLOCK_SIZE];
            self.read_block(block, &mut buf)?;
            let copy_len = cmp::min(BLOCK_SIZE, remaining);
            data[offset..offset + copy_len].copy_from_slice(&buf[..copy_len]);
            offset += copy_len;
            remaining -= copy_len;
            block_idx += 1;
        }

        Ok(data)
    }

    pub fn write_file(&mut self, _dir_inode: u32, entry: &DirEntryInfo, contents: &[u8]) -> Result<(), &'static str> {
        let mut inode = self.read_inode(entry.inode)?;
        if !inode.is_file() {
            return Err("Not a file.");
        }

        let per_block = (self.block_size / 4) as usize;
        let max_blocks = 12 + per_block + (per_block * per_block);
        let needed = if contents.is_empty() {
            0
        } else {
            (contents.len() + BLOCK_SIZE - 1) / BLOCK_SIZE
        };
        if needed > max_blocks {
            return Err("File too large for EXT2 driver.");
        }

        self.free_inode_blocks(&inode)?;

        let mut data_blocks = Vec::new();
        if needed > 0 {
            for _ in 0..needed {
                data_blocks.push(self.allocate_block()?);
            }
        }

        let mut indirect_blocks = 0u32;
        self.assign_blocks(&mut inode, &data_blocks, &mut indirect_blocks)?;

        let mut remaining = contents.len();
        let mut offset = 0usize;
        for block in data_blocks {
            let mut buf = vec![0u8; BLOCK_SIZE];
            let copy_len = cmp::min(BLOCK_SIZE, remaining);
            if copy_len > 0 {
                buf[..copy_len].copy_from_slice(&contents[offset..offset + copy_len]);
                offset += copy_len;
                remaining -= copy_len;
            }
            self.write_block(block, &buf)?;
            if remaining == 0 {
                break;
            }
        }

        inode.size = contents.len() as u32;
        let block_units = (needed as u32 + indirect_blocks) * BLOCK_SECTORS;
        inode.blocks = block_units;
        let now = time::current_time_secs().unwrap_or(0) as u32;
        inode.mtime = now;
        inode.ctime = now;

        self.write_inode(entry.inode, &inode)
    }

    pub fn delete_entry(&mut self, dir_inode: u32, entry: &DirEntryInfo) -> Result<(), &'static str> {
        let inode = self.read_inode(entry.inode)?;
        if inode.is_dir() {
            return Err("Not a file.");
        }

        self.free_inode_blocks(&inode)?;
        self.free_inode(entry.inode)?;
        self.clear_dir_entry(dir_inode, entry)
    }

    pub fn delete_dir(&mut self, dir_inode: u32, entry: &DirEntryInfo) -> Result<(), &'static str> {
        let inode = self.read_inode(entry.inode)?;
        if !inode.is_dir() {
            return Err("Not a directory.");
        }
        if !self.dir_is_empty(entry.inode)? {
            return Err("Directory not empty.");
        }

        self.free_inode_blocks(&inode)?;
        self.free_inode(entry.inode)?;
        self.clear_dir_entry(dir_inode, entry)?;

        let mut parent = self.read_inode(dir_inode)?;
        parent.links_count = parent.links_count.saturating_sub(1);
        self.write_inode(dir_inode, &parent)
    }

    pub fn update_access_date(&mut self, entry: &DirEntryInfo) -> Result<(), &'static str> {
        let mut inode = self.read_inode(entry.inode)?;
        let now = time::current_time_secs().unwrap_or(0) as u32;
        inode.atime = now;
        self.write_inode(entry.inode, &inode)
    }

    fn read_block(&self, block: u32, buf: &mut [u8]) -> Result<(), &'static str> {
        if buf.len() != BLOCK_SIZE {
            return Err("Invalid block buffer.");
        }
        read_block_raw(&self.device, self.part_start, block, buf)
    }

    fn write_block(&self, block: u32, buf: &[u8]) -> Result<(), &'static str> {
        if buf.len() != BLOCK_SIZE {
            return Err("Invalid block buffer.");
        }
        write_block_raw(&self.device, self.part_start, block, buf)
    }

    fn read_inode(&mut self, inode: u32) -> Result<Inode, &'static str> {
        if inode == 0 || inode > self.inodes_count {
            return Err("Invalid inode.");
        }
        let group = self.group_index_for_inode(inode);
        let index = (inode - 1) % self.inodes_per_group;
        let desc = self.group_desc[group as usize];
        let inode_size = self.inode_size as u32;
        let block_offset = (index * inode_size) / self.block_size;
        let offset = (index * inode_size) % self.block_size;
        let mut buf = vec![0u8; BLOCK_SIZE];
        self.read_block(desc.inode_table + block_offset, &mut buf)?;
        parse_inode(&buf, offset as usize)
    }

    fn write_inode(&mut self, inode: u32, entry: &Inode) -> Result<(), &'static str> {
        if inode == 0 || inode > self.inodes_count {
            return Err("Invalid inode.");
        }
        let group = self.group_index_for_inode(inode);
        let index = (inode - 1) % self.inodes_per_group;
        let desc = self.group_desc[group as usize];
        let inode_size = self.inode_size as u32;
        let block_offset = (index * inode_size) / self.block_size;
        let offset = (index * inode_size) % self.block_size;
        let mut buf = vec![0u8; BLOCK_SIZE];
        self.read_block(desc.inode_table + block_offset, &mut buf)?;
        write_inode(&mut buf, offset as usize, entry);
        self.write_block(desc.inode_table + block_offset, &buf)
    }

    fn inode_block(&mut self, inode: &Inode, block_index: u32) -> Result<u32, &'static str> {
        let per_block = self.block_size / 4;
        if block_index < 12 {
            return Ok(inode.block[block_index as usize]);
        }
        let mut idx = block_index - 12;
        if idx < per_block {
            return self.read_indirect_block(inode.block[12], idx);
        }
        idx -= per_block;
        let per_double = per_block * per_block;
        if idx < per_double {
            let outer = idx / per_block;
            let inner = idx % per_block;
            let first = self.read_indirect_block(inode.block[13], outer)?;
            return self.read_indirect_block(first, inner);
        }
        Err("File too large for EXT2 driver.")
    }

    fn assign_blocks(&mut self, inode: &mut Inode, blocks: &[u32], indirect_blocks: &mut u32) -> Result<(), &'static str> {
        for slot in inode.block.iter_mut() {
            *slot = 0;
        }

        let per_block = self.block_size / 4;
        let mut remaining = blocks;

        for (idx, &block) in remaining.iter().take(12).enumerate() {
            inode.block[idx] = block;
        }
        if remaining.len() <= 12 {
            return Ok(());
        }
        remaining = &remaining[12..];

        let mut single = vec![0u8; BLOCK_SIZE];
        let single_block = self.allocate_block()?;
        *indirect_blocks = indirect_blocks.saturating_add(1);
        for (idx, &block) in remaining.iter().take(per_block as usize).enumerate() {
            write_u32(&mut single, idx * 4, block);
        }
        self.write_block(single_block, &single)?;
        inode.block[12] = single_block;

        if remaining.len() <= per_block as usize {
            return Ok(());
        }
        remaining = &remaining[per_block as usize..];

        let double_block = self.allocate_block()?;
        *indirect_blocks = indirect_blocks.saturating_add(1);
        let mut double = vec![0u8; BLOCK_SIZE];
        let mut used = 0usize;

        while used < remaining.len() {
            let chunk = cmp::min(per_block as usize, remaining.len() - used);
            let mut indirect = vec![0u8; BLOCK_SIZE];
            let indirect_block = self.allocate_block()?;
            *indirect_blocks = indirect_blocks.saturating_add(1);
            for (idx, &block) in remaining[used..used + chunk].iter().enumerate() {
                write_u32(&mut indirect, idx * 4, block);
            }
            self.write_block(indirect_block, &indirect)?;
            write_u32(&mut double, (used / per_block as usize) * 4, indirect_block);
            used += chunk;
        }

        self.write_block(double_block, &double)?;
        inode.block[13] = double_block;

        if used < remaining.len() {
            return Err("File too large for EXT2 driver.");
        }

        Ok(())
    }

    fn read_indirect_block(&mut self, block: u32, index: u32) -> Result<u32, &'static str> {
        if block == 0 {
            return Ok(0);
        }
        let mut buf = vec![0u8; BLOCK_SIZE];
        self.read_block(block, &mut buf)?;
        let offset = (index * 4) as usize;
        if offset + 4 > BLOCK_SIZE {
            return Err("Invalid EXT2 block index.");
        }
        Ok(read_u32(&buf, offset))
    }

    fn allocate_block(&mut self) -> Result<u32, &'static str> {
        for group in 0..self.groups_count {
            if self.group_desc[group as usize].free_blocks_count == 0 {
                continue;
            }
            let desc = self.group_desc[group as usize];
            let mut bitmap = vec![0u8; BLOCK_SIZE];
            self.read_block(desc.block_bitmap, &mut bitmap)?;
            let blocks_in_group = blocks_in_group(self.blocks_count, self.blocks_per_group, group);
            for bit in 0..blocks_in_group {
                if !test_bit(&bitmap, bit) {
                    set_bit(&mut bitmap, bit);
                    self.write_block(desc.block_bitmap, &bitmap)?;
                    self.group_desc[group as usize].free_blocks_count = self.group_desc[group as usize]
                        .free_blocks_count
                        .saturating_sub(1);
                    self.free_blocks_count = self.free_blocks_count.saturating_sub(1);
                    write_group_descs(&self.device, self.part_start, self.groups_count, &self.group_desc)?;
                    write_superblock(&self.device, self.part_start, &self.superblock_state())?;
                    return Ok(group * self.blocks_per_group + bit);
                }
            }
        }
        Err("No free blocks.")
    }

    fn free_block(&mut self, block: u32) -> Result<(), &'static str> {
        let group = block / self.blocks_per_group;
        let bit = block % self.blocks_per_group;
        if group >= self.groups_count {
            return Err("Invalid block.");
        }
        let desc = self.group_desc[group as usize];
        let mut bitmap = vec![0u8; BLOCK_SIZE];
        self.read_block(desc.block_bitmap, &mut bitmap)?;
        if test_bit(&bitmap, bit) {
            clear_bit(&mut bitmap, bit);
            self.write_block(desc.block_bitmap, &bitmap)?;
            self.group_desc[group as usize].free_blocks_count = self.group_desc[group as usize]
                .free_blocks_count
                .saturating_add(1);
            self.free_blocks_count = self.free_blocks_count.saturating_add(1);
            write_group_descs(&self.device, self.part_start, self.groups_count, &self.group_desc)?;
            write_superblock(&self.device, self.part_start, &self.superblock_state())?;
        }
        Ok(())
    }

    fn allocate_inode(&mut self, is_dir: bool) -> Result<u32, &'static str> {
        for group in 0..self.groups_count {
            if self.group_desc[group as usize].free_inodes_count == 0 {
                continue;
            }
            let desc = self.group_desc[group as usize];
            let mut bitmap = vec![0u8; BLOCK_SIZE];
            self.read_block(desc.inode_bitmap, &mut bitmap)?;
            for bit in 0..self.inodes_per_group {
                let inode = group * self.inodes_per_group + bit + 1;
                if inode < EXT2_FIRST_INO {
                    continue;
                }
                if !test_bit(&bitmap, bit) {
                    set_bit(&mut bitmap, bit);
                    self.write_block(desc.inode_bitmap, &bitmap)?;
                    self.group_desc[group as usize].free_inodes_count = self.group_desc[group as usize]
                        .free_inodes_count
                        .saturating_sub(1);
                    if is_dir {
                        self.group_desc[group as usize].used_dirs_count = self.group_desc[group as usize]
                            .used_dirs_count
                            .saturating_add(1);
                    }
                    self.free_inodes_count = self.free_inodes_count.saturating_sub(1);
                    write_group_descs(&self.device, self.part_start, self.groups_count, &self.group_desc)?;
                    write_superblock(&self.device, self.part_start, &self.superblock_state())?;
                    return Ok(inode);
                }
            }
        }
        Err("No free inodes.")
    }

    fn free_inode(&mut self, inode: u32) -> Result<(), &'static str> {
        let group = self.group_index_for_inode(inode);
        let bit = (inode - 1) % self.inodes_per_group;
        let desc = self.group_desc[group as usize];
        let mut bitmap = vec![0u8; BLOCK_SIZE];
        self.read_block(desc.inode_bitmap, &mut bitmap)?;
        if test_bit(&bitmap, bit) {
            clear_bit(&mut bitmap, bit);
            self.write_block(desc.inode_bitmap, &bitmap)?;
            self.group_desc[group as usize].free_inodes_count = self.group_desc[group as usize]
                .free_inodes_count
                .saturating_add(1);
            self.free_inodes_count = self.free_inodes_count.saturating_add(1);
            write_group_descs(&self.device, self.part_start, self.groups_count, &self.group_desc)?;
            write_superblock(&self.device, self.part_start, &self.superblock_state())?;
        }
        Ok(())
    }

    fn free_inode_blocks(&mut self, inode: &Inode) -> Result<(), &'static str> {
        for &block in inode.block.iter().take(12) {
            if block != 0 {
                self.free_block(block)?;
            }
        }
        if inode.block[12] != 0 {
            self.free_indirect_chain(inode.block[12], 1)?;
        }
        if inode.block[13] != 0 {
            self.free_indirect_chain(inode.block[13], 2)?;
        }
        if inode.block[14] != 0 {
            self.free_indirect_chain(inode.block[14], 3)?;
        }
        Ok(())
    }

    fn free_indirect_chain(&mut self, block: u32, depth: u8) -> Result<(), &'static str> {
        if block == 0 {
            return Ok(());
        }
        if depth == 0 {
            self.free_block(block)?;
            return Ok(());
        }
        let mut buf = vec![0u8; BLOCK_SIZE];
        self.read_block(block, &mut buf)?;
        let entries = (self.block_size / 4) as usize;
        for idx in 0..entries {
            let ptr = read_u32(&buf, idx * 4);
            if ptr != 0 {
                self.free_indirect_chain(ptr, depth - 1)?;
            }
        }
        self.free_block(block)
    }

    fn insert_dir_entry(&mut self, dir_inode: u32, name: &str, inode: u32, file_type: u8) -> Result<(), &'static str> {
        let mut dir = self.read_inode(dir_inode)?;
        let entry_size = align4(8 + name.len());
        let mut block_idx = 0u32;
        let mut remaining = dir.size as u64;

        while remaining > 0 {
            let block = self.inode_block(&dir, block_idx)?;
            if block == 0 {
                break;
            }
            let mut buf = vec![0u8; BLOCK_SIZE];
            self.read_block(block, &mut buf)?;
            let mut offset = 0usize;
            while offset + 8 <= BLOCK_SIZE {
                let rec_len = read_u16(&buf, offset + 4) as usize;
                let name_len = buf[offset + 6] as usize;
                if rec_len == 0 || offset + rec_len > BLOCK_SIZE {
                    break;
                }
                let used = align4(8 + name_len);
                if rec_len >= used + entry_size {
                    write_u16(&mut buf, offset + 4, used as u16);
                    let new_off = offset + used;
                    write_u32(&mut buf, new_off, inode);
                    write_u16(&mut buf, new_off + 4, (rec_len - used) as u16);
                    buf[new_off + 6] = name.len() as u8;
                    buf[new_off + 7] = file_type;
                    let name_bytes = name.as_bytes();
                    buf[new_off + 8..new_off + 8 + name_bytes.len()].copy_from_slice(name_bytes);
                    self.write_block(block, &buf)?;
                    return Ok(());
                }
                offset += rec_len;
            }
            block_idx += 1;
            remaining = remaining.saturating_sub(BLOCK_SIZE as u64);
        }

        let new_block = self.allocate_block()?;
        let mut buf = vec![0u8; BLOCK_SIZE];
        write_u32(&mut buf, 0, inode);
        write_u16(&mut buf, 4, BLOCK_SIZE as u16);
        buf[6] = name.len() as u8;
        buf[7] = file_type;
        let name_bytes = name.as_bytes();
        buf[8..8 + name_bytes.len()].copy_from_slice(name_bytes);
        self.write_block(new_block, &buf)?;

        self.set_inode_block(&mut dir, block_idx, new_block)?;
        dir.size = dir.size.saturating_add(BLOCK_SIZE as u32);
        dir.blocks = dir.blocks.saturating_add(BLOCK_SECTORS);
        self.write_inode(dir_inode, &dir)
    }

    fn set_inode_block(&mut self, inode: &mut Inode, block_index: u32, block: u32) -> Result<(), &'static str> {
        let per_block = self.block_size / 4;
        if block_index < 12 {
            inode.block[block_index as usize] = block;
            return Ok(());
        }
        let mut idx = block_index - 12;
        if idx < per_block {
            if inode.block[12] == 0 {
                inode.block[12] = self.allocate_block()?;
            }
            self.write_indirect_entry(inode.block[12], idx, block)?;
            return Ok(());
        }
        idx -= per_block;
        let per_double = per_block * per_block;
        if idx < per_double {
            if inode.block[13] == 0 {
                inode.block[13] = self.allocate_block()?;
            }
            let outer = idx / per_block;
            let inner = idx % per_block;
            let first = self.read_indirect_block(inode.block[13], outer)?;
            let indirect = if first == 0 {
                let new_block = self.allocate_block()?;
                self.write_indirect_entry(inode.block[13], outer, new_block)?;
                new_block
            } else {
                first
            };
            self.write_indirect_entry(indirect, inner, block)?;
            return Ok(());
        }
        Err("Directory too large for EXT2 driver.")
    }

    fn write_indirect_entry(&mut self, block: u32, index: u32, value: u32) -> Result<(), &'static str> {
        let mut buf = vec![0u8; BLOCK_SIZE];
        self.read_block(block, &mut buf)?;
        let offset = (index * 4) as usize;
        if offset + 4 > BLOCK_SIZE {
            return Err("Invalid EXT2 block index.");
        }
        write_u32(&mut buf, offset, value);
        self.write_block(block, &buf)
    }

    fn clear_dir_entry(&mut self, dir_inode: u32, entry: &DirEntryInfo) -> Result<(), &'static str> {
        let mut buf = vec![0u8; BLOCK_SIZE];
        self.read_block(entry.block, &mut buf)?;
        write_u32(&mut buf, entry.offset as usize, 0);
        self.write_block(entry.block, &buf)?;
        let _ = dir_inode;
        Ok(())
    }

    fn dir_is_empty(&mut self, inode: u32) -> Result<bool, &'static str> {
        let entries = self.read_directory(inode)?;
        for entry in entries {
            if entry.name != "." && entry.name != ".." {
                return Ok(false);
            }
        }
        Ok(true)
    }

    fn write_directory_block(&mut self, block: u32, inode: u32, parent: u32) -> Result<(), &'static str> {
        let mut buf = vec![0u8; BLOCK_SIZE];
        write_u32(&mut buf, 0, inode);
        write_u16(&mut buf, 4, 12);
        buf[6] = 1;
        buf[7] = EXT2_FT_DIR;
        buf[8] = b'.';

        write_u32(&mut buf, 12, parent);
        write_u16(&mut buf, 16, (BLOCK_SIZE - 12) as u16);
        buf[18] = 2;
        buf[19] = EXT2_FT_DIR;
        buf[20] = b'.';
        buf[21] = b'.';

        self.write_block(block, &buf)
    }

    fn write_root_dir(&mut self, block: u32) -> Result<(), &'static str> {
        self.write_directory_block(block, ROOT_INODE, ROOT_INODE)
    }

    fn group_index_for_inode(&self, inode: u32) -> u32 {
        (inode - 1) / self.inodes_per_group
    }

    fn superblock_state(&self) -> Superblock {
        Superblock {
            inodes_count: self.inodes_count,
            blocks_count: self.blocks_count,
            free_blocks_count: self.free_blocks_count,
            free_inodes_count: self.free_inodes_count,
            first_data_block: self.first_data_block,
            log_block_size: 2,
            blocks_per_group: self.blocks_per_group,
            inodes_per_group: self.inodes_per_group,
            mtime: 0,
            wtime: time::current_time_secs().unwrap_or(0) as u32,
            mnt_count: 0,
            max_mnt_count: 0xFFFF,
            magic: EXT2_SUPER_MAGIC,
            state: 1,
            errors: 1,
            rev_level: EXT2_REV_DYNAMIC,
            first_ino: EXT2_FIRST_INO,
            inode_size: INODE_SIZE,
            volume_name: self.volume_name,
        }
    }
}

fn parse_superblock(block: &[u8]) -> Result<Superblock, &'static str> {
    if block.len() < 1024 + 200 {
        return Err("Invalid EXT2 superblock buffer.");
    }
    let base = 1024;
    let inodes_count = read_u32(block, base + 0);
    let blocks_count = read_u32(block, base + 4);
    let free_blocks_count = read_u32(block, base + 12);
    let free_inodes_count = read_u32(block, base + 16);
    let first_data_block = read_u32(block, base + 20);
    let log_block_size = read_u32(block, base + 24);
    let blocks_per_group = read_u32(block, base + 32);
    let inodes_per_group = read_u32(block, base + 40);
    let mtime = read_u32(block, base + 44);
    let wtime = read_u32(block, base + 48);
    let mnt_count = read_u16(block, base + 52);
    let max_mnt_count = read_u16(block, base + 54);
    let magic = read_u16(block, base + 56);
    let state = read_u16(block, base + 58);
    let errors = read_u16(block, base + 60);
    let rev_level = read_u32(block, base + 76);
    let first_ino = read_u32(block, base + 84);
    let inode_size = read_u16(block, base + 88);
    let mut volume_name = [0u8; 16];
    volume_name.copy_from_slice(&block[base + 120..base + 136]);

    Ok(Superblock {
        inodes_count,
        blocks_count,
        free_blocks_count,
        free_inodes_count,
        first_data_block,
        log_block_size,
        blocks_per_group,
        inodes_per_group,
        mtime,
        wtime,
        mnt_count,
        max_mnt_count,
        magic,
        state,
        errors,
        rev_level,
        first_ino,
        inode_size,
        volume_name,
    })
}

fn write_superblock<D: BlockDevice>(dev: &D, part_start: u32, sb: &Superblock) -> Result<(), &'static str> {
    let mut block0 = vec![0u8; BLOCK_SIZE];
    read_block_raw(dev, part_start, 0, &mut block0)?;
    let base = 1024;
    write_u32(&mut block0, base + 0, sb.inodes_count);
    write_u32(&mut block0, base + 4, sb.blocks_count);
    write_u32(&mut block0, base + 12, sb.free_blocks_count);
    write_u32(&mut block0, base + 16, sb.free_inodes_count);
    write_u32(&mut block0, base + 20, sb.first_data_block);
    write_u32(&mut block0, base + 24, sb.log_block_size);
    write_u32(&mut block0, base + 32, sb.blocks_per_group);
    write_u32(&mut block0, base + 40, sb.inodes_per_group);
    write_u32(&mut block0, base + 44, sb.mtime);
    write_u32(&mut block0, base + 48, sb.wtime);
    write_u16(&mut block0, base + 52, sb.mnt_count);
    write_u16(&mut block0, base + 54, sb.max_mnt_count);
    write_u16(&mut block0, base + 56, sb.magic);
    write_u16(&mut block0, base + 58, sb.state);
    write_u16(&mut block0, base + 60, sb.errors);
    write_u32(&mut block0, base + 76, sb.rev_level);
    write_u32(&mut block0, base + 84, sb.first_ino);
    write_u16(&mut block0, base + 88, sb.inode_size);
    block0[base + 120..base + 136].copy_from_slice(&sb.volume_name);
    write_block_raw(dev, part_start, 0, &block0)
}

fn read_group_descs<D: BlockDevice>(dev: &D, part_start: u32, groups: u32) -> Result<Vec<GroupDesc>, &'static str> {
    let total_bytes = groups as usize * 32;
    let mut buf = vec![0u8; ((total_bytes + BLOCK_SIZE - 1) / BLOCK_SIZE) * BLOCK_SIZE];
    for (i, chunk) in buf.chunks_mut(BLOCK_SIZE).enumerate() {
        read_block_raw(dev, part_start, 1 + i as u32, chunk)?;
    }

    let mut descs = Vec::new();
    for i in 0..groups as usize {
        let base = i * 32;
        let block_bitmap = read_u32(&buf, base);
        let inode_bitmap = read_u32(&buf, base + 4);
        let inode_table = read_u32(&buf, base + 8);
        let free_blocks_count = read_u16(&buf, base + 12);
        let free_inodes_count = read_u16(&buf, base + 14);
        let used_dirs_count = read_u16(&buf, base + 16);
        descs.push(GroupDesc {
            block_bitmap,
            inode_bitmap,
            inode_table,
            free_blocks_count,
            free_inodes_count,
            used_dirs_count,
        });
    }
    Ok(descs)
}

fn write_group_descs<D: BlockDevice>(
    dev: &D,
    part_start: u32,
    groups: u32,
    descs: &[GroupDesc],
) -> Result<(), &'static str> {
    let total_bytes = groups as usize * 32;
    let mut buf = vec![0u8; ((total_bytes + BLOCK_SIZE - 1) / BLOCK_SIZE) * BLOCK_SIZE];
    for (i, desc) in descs.iter().enumerate() {
        let base = i * 32;
        write_u32(&mut buf, base, desc.block_bitmap);
        write_u32(&mut buf, base + 4, desc.inode_bitmap);
        write_u32(&mut buf, base + 8, desc.inode_table);
        write_u16(&mut buf, base + 12, desc.free_blocks_count);
        write_u16(&mut buf, base + 14, desc.free_inodes_count);
        write_u16(&mut buf, base + 16, desc.used_dirs_count);
    }

    for (i, chunk) in buf.chunks(BLOCK_SIZE).enumerate() {
        write_block_raw(dev, part_start, 1 + i as u32, chunk)?;
    }

    Ok(())
}

fn parse_inode(buf: &[u8], offset: usize) -> Result<Inode, &'static str> {
    if offset + INODE_SIZE as usize > buf.len() {
        return Err("Invalid inode offset.");
    }
    let mode = read_u16(buf, offset + 0);
    let uid = read_u16(buf, offset + 2);
    let size = read_u32(buf, offset + 4);
    let atime = read_u32(buf, offset + 8);
    let ctime = read_u32(buf, offset + 12);
    let mtime = read_u32(buf, offset + 16);
    let dtime = read_u32(buf, offset + 20);
    let gid = read_u16(buf, offset + 24);
    let links_count = read_u16(buf, offset + 26);
    let blocks = read_u32(buf, offset + 28);
    let flags = read_u32(buf, offset + 32);
    let mut block = [0u32; 15];
    for i in 0..15 {
        block[i] = read_u32(buf, offset + 40 + i * 4);
    }

    Ok(Inode {
        mode,
        uid,
        size,
        atime,
        ctime,
        mtime,
        dtime,
        gid,
        links_count,
        blocks,
        flags,
        block,
    })
}

fn write_inode(buf: &mut [u8], offset: usize, inode: &Inode) {
    write_u16(buf, offset + 0, inode.mode);
    write_u16(buf, offset + 2, inode.uid);
    write_u32(buf, offset + 4, inode.size);
    write_u32(buf, offset + 8, inode.atime);
    write_u32(buf, offset + 12, inode.ctime);
    write_u32(buf, offset + 16, inode.mtime);
    write_u32(buf, offset + 20, inode.dtime);
    write_u16(buf, offset + 24, inode.gid);
    write_u16(buf, offset + 26, inode.links_count);
    write_u32(buf, offset + 28, inode.blocks);
    write_u32(buf, offset + 32, inode.flags);
    for i in 0..15 {
        write_u32(buf, offset + 40 + i * 4, inode.block[i]);
    }
}

fn read_block_raw<D: BlockDevice>(dev: &D, part_start: u32, block: u32, buf: &mut [u8]) -> Result<(), &'static str> {
    let lba_start = part_start
        .saturating_add(block.saturating_mul(BLOCK_SECTORS));
    for (idx, chunk) in buf.chunks_mut(SECTOR_SIZE).enumerate() {
        let lba = lba_start.saturating_add(idx as u32);
        dev.read_block(lba as u64, chunk).map_err(|err| err.as_str())?;
    }
    Ok(())
}

fn write_block_raw<D: BlockDevice>(dev: &D, part_start: u32, block: u32, buf: &[u8]) -> Result<(), &'static str> {
    let lba_start = part_start
        .saturating_add(block.saturating_mul(BLOCK_SECTORS));
    for (idx, chunk) in buf.chunks(SECTOR_SIZE).enumerate() {
        let lba = lba_start.saturating_add(idx as u32);
        dev.write_block(lba as u64, chunk).map_err(|err| err.as_str())?;
    }
    Ok(())
}

fn write_mbr<D: BlockDevice>(dev: &D, part_start: u32, part_sectors: u32) -> Result<(), &'static str> {
    let mut sector = [0u8; SECTOR_SIZE];
    let signature = time::current_time_secs().unwrap_or(0) as u32;
    sector[440..444].copy_from_slice(&signature.to_le_bytes());
    let base = 446;
    sector[base + 4] = EXT2_PART_TYPE;
    sector[base + 8..base + 12].copy_from_slice(&part_start.to_le_bytes());
    sector[base + 12..base + 16].copy_from_slice(&part_sectors.to_le_bytes());
    sector[510] = 0x55;
    sector[511] = 0xAA;
    dev.write_block(0, &sector).map_err(|err| err.as_str())
}

fn build_volume_name(label: &str) -> [u8; 16] {
    let mut out = [0u8; 16];
    let trimmed = label.trim();
    let bytes = trimmed.as_bytes();
    let len = cmp::min(bytes.len(), 16);
    out[..len].copy_from_slice(&bytes[..len]);
    out
}

fn compute_inodes_per_group(blocks_per_group: u32) -> u32 {
    let bytes = blocks_per_group as u64 * BLOCK_SIZE as u64;
    let mut inodes = (bytes / 16384) as u32;
    if inodes < 128 {
        inodes = 128;
    }
    inodes - (inodes % 8)
}

fn inode_table_blocks(inodes_per_group: u32) -> u32 {
    let bytes = inodes_per_group as u64 * INODE_SIZE as u64;
    ((bytes + BLOCK_SIZE as u64 - 1) / BLOCK_SIZE as u64) as u32
}

fn root_dir_block(inode_table_blocks: u32) -> u32 {
    4 + inode_table_blocks
}

fn blocks_in_group(total_blocks: u32, blocks_per_group: u32, group: u32) -> u32 {
    let start = group * blocks_per_group;
    let remaining = total_blocks.saturating_sub(start);
    cmp::min(remaining, blocks_per_group)
}

fn align4(value: usize) -> usize {
    (value + 3) & !3
}

fn test_bit(buf: &[u8], bit: u32) -> bool {
    let byte = (bit / 8) as usize;
    let mask = 1u8 << (bit % 8);
    if byte >= buf.len() {
        return false;
    }
    buf[byte] & mask != 0
}

fn set_bit(buf: &mut [u8], bit: u32) {
    let byte = (bit / 8) as usize;
    let mask = 1u8 << (bit % 8);
    if byte < buf.len() {
        buf[byte] |= mask;
    }
}

fn clear_bit(buf: &mut [u8], bit: u32) {
    let byte = (bit / 8) as usize;
    let mask = 1u8 << (bit % 8);
    if byte < buf.len() {
        buf[byte] &= !mask;
    }
}

fn read_u16(buf: &[u8], offset: usize) -> u16 {
    let mut b = [0u8; 2];
    b.copy_from_slice(&buf[offset..offset + 2]);
    u16::from_le_bytes(b)
}

fn read_u32(buf: &[u8], offset: usize) -> u32 {
    let mut b = [0u8; 4];
    b.copy_from_slice(&buf[offset..offset + 4]);
    u32::from_le_bytes(b)
}

fn write_u16(buf: &mut [u8], offset: usize, value: u16) {
    buf[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn write_u32(buf: &mut [u8], offset: usize, value: u32) {
    buf[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
