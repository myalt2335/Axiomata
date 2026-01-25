use crate::{cdmo, console, ext2, fat32, fs, run_mode};
use alloc::format;
use crate::console::CompositorMode;

const DEV_CFG_PATH: &str = "\\dev\\cfg.cfg";
const DEV_CFG_TOKEN_DEBUG: &str = "debug_assertions";
const DEV_CFG_TOKEN_TERMINAL_OS: &str = "terminal_os_commands";

pub fn is_available() -> bool {
    dev_cfg_has_token(DEV_CFG_TOKEN_DEBUG)
}

pub fn terminal_os_commands_enabled() -> bool {
    dev_cfg_has_token(DEV_CFG_TOKEN_TERMINAL_OS)
}

fn dev_cfg_has_token(token: &str) -> bool {
    let Some(contents) = fs::read_file(DEV_CFG_PATH) else {
        return false;
    };
    let normalized = contents.replace("\r\n", "\n");
    for line in normalized.lines() {
        let mut trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("cfg(") {
            trimmed = rest;
        }
        trimmed = trimmed.trim_end_matches(')');
        for part in trimmed.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            if part == token {
                return true;
            }
        }
    }
    false
}

pub fn command(args: &[&str]) {
    if args.is_empty() {
        usage();
        return;
    }

    match args[0] {
        "compdemo" => cdmo::command(&args[1..]),
        "fbffer" => {
            match args.len() {
                1 => toggle_back_buffer(),
                2 => match args[1] {
                    "back_buffer" => toggle_back_buffer(),
                    "scene_buffer" => toggle_scene_buffer(),
                    _ => usage(),
                },
                _ => usage(),
            }
        }
        "fbmode" => {
            if args.len() != 2 {
                usage();
                return;
            }
            match args[1] {
                "legacy" => {
                    console::set_classic_mode(false);
                    console::set_compositor_mode(CompositorMode::Legacy);
                    console::write_line("fbmode: legacy");
                }
                "layered" => {
                    console::set_classic_mode(false);
                    console::set_compositor_mode(CompositorMode::Layered);
                    if console::compositor_mode() == CompositorMode::Layered {
                        console::write_line("fbmode: layered");
                    } else {
                        console::write_line("fbmode: layered unavailable");
                    }
                }
                "classic" => {
                    console::set_classic_mode(true);
                    console::set_compositor_mode(CompositorMode::Legacy);
                    let _ = console::set_double_buffering(false);
                    console::write_line("fbmode: classic");
                }
                _ => {
                    usage();
                }
            }
        }
        "mode" => {
            match args.len() {
                1 => toggle_run_mode(),
                2 => match args[1] {
                    "toggle" => toggle_run_mode(),
                    "console" => set_run_mode(run_mode::RunMode::Console),
                    "desktop" => set_run_mode(run_mode::RunMode::Desktop),
                    _ => usage(),
                },
                _ => usage(),
            }
        }
        "compinfo" => {
            if args.len() != 1 {
                usage();
                return;
            }
            let classic =
                console::is_classic_mode() && console::compositor_mode() == CompositorMode::Legacy;
            let mode = if classic {
                "classic"
            } else {
                match console::compositor_mode() {
                    CompositorMode::Legacy => "legacy",
                    CompositorMode::Layered => "layered",
                }
            };
            let double_buffered = console::is_double_buffered();
            let scene_buffer_active =
                console::compositor_mode() == CompositorMode::Layered && console::has_scene_buffer();
            console::write_line(&format!(
                "compinfo: mode={}, double_buffered={}, scene_buffer_active={}",
                mode,
                double_buffered,
                scene_buffer_active
            ));
        }
        "filesystem" => {
            if args.len() != 1 {
                usage();
                return;
            }
            filesystem_info();
        }
        _ => usage(),
    }
}

fn usage() {
    console::write_line("Usage: debug compdemo [toggle]");
    console::write_line("       debug fbffer [back_buffer|scene_buffer]");
    console::write_line("       debug fbmode legacy|layered|classic");
    console::write_line("       debug mode [toggle|console|desktop]");
    console::write_line("       debug compinfo");
    console::write_line("       debug filesystem");
}

fn toggle_back_buffer() {
    match console::toggle_double_buffering() {
        Ok(true) => console::write_line("fbffer: double buffer on"),
        Ok(false) => console::write_line("fbffer: double buffer off"),
        Err(msg) => console::write_line(msg),
    }
}

fn toggle_scene_buffer() {
    console::set_classic_mode(false);
    let target = match console::compositor_mode() {
        CompositorMode::Legacy => CompositorMode::Layered,
        CompositorMode::Layered => CompositorMode::Legacy,
    };
    console::set_compositor_mode(target);
    let active = console::compositor_mode();
    if active != target && target == CompositorMode::Layered {
        console::write_line("fbffer: layered unavailable");
        return;
    }
    let label = match active {
        CompositorMode::Legacy => "legacy",
        CompositorMode::Layered => "layered",
    };
    console::write_line(&format!("fbffer: {}", label));
}

fn toggle_run_mode() {
    let target = match run_mode::current() {
        run_mode::RunMode::Console => run_mode::RunMode::Desktop,
        run_mode::RunMode::Desktop => run_mode::RunMode::Console,
    };
    set_run_mode(target);
}

fn set_run_mode(target: run_mode::RunMode) {
    let current = run_mode::current();
    if current == target {
        console::write_line(&format!("mode: already in {}", mode_label(target)));
        return;
    }
    if matches!(target, run_mode::RunMode::Desktop) && !console::has_scene_buffer() {
        console::write_line("mode: desktop unavailable (no scene buffer).");
        return;
    }
    run_mode::request(target);
    console::write_line(&format!("mode: switching to {}", mode_label(target)));
}

fn mode_label(mode: run_mode::RunMode) -> &'static str {
    match mode {
        run_mode::RunMode::Console => "console",
        run_mode::RunMode::Desktop => "desktop",
    }
}

fn filesystem_info() {
    let info = fs::persist_info();
    let fs_kind = match info.fs_kind {
        Some(fs::FsKind::Fat32) => "fat32",
        Some(fs::FsKind::Ext2) => "ext2",
        None => "none",
    };
    let preferred = match info.preferred_fs {
        fs::FsPreference::Auto => "auto",
        fs::FsPreference::Fat32 => "fat32",
        fs::FsPreference::Ext2 => "ext2",
    };
    let drive = match info.drive {
        Some(crate::ata::DriveSelect::PrimaryMaster) => "ATA0 master",
        Some(crate::ata::DriveSelect::PrimarySlave) => "ATA0 slave",
        Some(crate::ata::DriveSelect::SecondaryMaster) => "ATA1 master",
        Some(crate::ata::DriveSelect::SecondarySlave) => "ATA1 slave",
        None => "unknown",
    };
    let bytes = info.sectors.saturating_mul(512);
    if info.drive.is_none() {
        console::write_line("Persistent filesystem: no disk selected.");
    } else if info.enabled {
        console::write_line(&format!(
            "Persistent filesystem: enabled ({}; {} sectors, {} bytes).",
            drive, info.sectors, bytes
        ));
    } else {
        console::write_line(&format!(
            "Persistent filesystem: disabled ({}; {} sectors, {} bytes).",
            drive, info.sectors, bytes
        ));
    }
    console::write_line(&format!(
        "Filesystem type: {} (preferred {}).",
        fs_kind, preferred
    ));

    if let Some(msg) = info.last_error {
        console::write_line(&format!("Last error: {}", msg));
    }

    if let Some(part) = info.partition {
        let part_bytes = part.sectors.saturating_mul(512) as u64;
        console::write_line(&format!(
            "Partition: type 0x{:02X} @ LBA {} ({} sectors, {} bytes).",
            part.type_code, part.lba_start, part.sectors, part_bytes
        ));
    }
    if let Some(fat) = info.fat32_info {
        let cluster_bytes = fat.bytes_per_sector as u32 * fat.sectors_per_cluster as u32;
        let label = core::str::from_utf8(&fat.volume_label)
            .unwrap_or("NO_LABEL")
            .trim();
        console::write_line(&format!(
            "FAT32: label '{}' (cluster {} bytes, FAT {} sectors).",
            label, cluster_bytes, fat.sectors_per_fat
        ));
        console::write_line(&format!(
            "FAT32 layout: reserved {} sectors, {} FATs, root cluster {}.",
            fat.reserved_sectors, fat.num_fats, fat.root_cluster
        ));
        let total_bytes = fat.total_sectors as u64 * fat.bytes_per_sector as u64;
        let data_sectors = fat
            .total_sectors
            .saturating_sub(fat.reserved_sectors as u32)
            .saturating_sub(fat.num_fats as u32 * fat.sectors_per_fat);
        let clusters = if fat.sectors_per_cluster == 0 {
            0
        } else {
            data_sectors / fat.sectors_per_cluster as u32
        };
        console::write_line(&format!(
            "FAT32 size: {} sectors ({} bytes), data clusters {}.",
            fat.total_sectors, total_bytes, clusters
        ));
        if let Some(part) = info.partition {
            let fat_start = part.lba_start + fat.reserved_sectors as u32;
            let data_start = fat_start + fat.num_fats as u32 * fat.sectors_per_fat;
            console::write_line(&format!(
                "FAT32 LBA: FAT @ {}, data @ {}.",
                fat_start, data_start
            ));
        }
    }
    if let Some(ext2_info) = info.ext2_info {
        let label = core::str::from_utf8(&ext2_info.volume_name)
            .unwrap_or("NO_LABEL")
            .trim();
        console::write_line(&format!(
            "EXT2: label '{}' (block {} bytes, inode {} bytes).",
            label, ext2_info.block_size, ext2_info.inode_size
        ));
        console::write_line(&format!(
            "EXT2 layout: {} groups, {} blocks/group, {} inodes/group.",
            ext2_info.groups_count, ext2_info.blocks_per_group, ext2_info.inodes_per_group
        ));
        console::write_line(&format!(
            "EXT2 size: {} blocks ({} free), {} inodes ({} free).",
            ext2_info.blocks_count,
            ext2_info.free_blocks_count,
            ext2_info.inodes_count,
            ext2_info.free_inodes_count
        ));
        console::write_line(&format!(
            "EXT2 data: first block {}, partition {} sectors.",
            ext2_info.first_data_block, ext2_info.part_sectors
        ));
    }

    fn err_label(err: crate::ata::AtaError) -> &'static str {
        match err {
            crate::ata::AtaError::NoDevice => "no device",
            crate::ata::AtaError::Timeout => "timeout",
            crate::ata::AtaError::Error => "error",
        }
    }

    fn probe_line(label: &str, probe: fs::ProbeResult) {
        match probe {
            fs::ProbeResult::NotTried => {
                console::write_line(&format!("{}: not probed.", label));
            }
            fs::ProbeResult::IdentifyError(err) => {
                console::write_line(&format!("{}: identify failed ({})", label, err_label(err)));
            }
            fs::ProbeResult::ReadError(err) => {
                console::write_line(&format!("{}: read failed ({})", label, err_label(err)));
            }
            fs::ProbeResult::Identified { sectors, mbr } => {
                let bytes = sectors.saturating_mul(512);
                let has_fat32 = fat32::find_fat32_partition(&mbr).is_some();
                let has_ext2 = ext2::find_ext2_partition(&mbr).is_some();
                let layout = if mbr.is_empty {
                    "empty"
                } else if !mbr.signature {
                    "data"
                } else if has_fat32 && has_ext2 {
                    "mbr/fat32+ext2"
                } else if has_fat32 {
                    "mbr/fat32"
                } else if has_ext2 {
                    "mbr/ext2"
                } else {
                    "mbr/no-fs"
                };
                console::write_line(&format!(
                    "{}: ok ({} sectors, {} bytes, {})",
                    label, sectors, bytes, layout
                ));
            }
        }
    }

    probe_line("ATA0 master", info.primary_master_probe);
    probe_line("ATA0 slave", info.primary_slave_probe);
    probe_line("ATA1 master", info.secondary_master_probe);
    probe_line("ATA1 slave", info.secondary_slave_probe);

    let ata_cfg = crate::ata::io_config();
    console::write_line(&format!(
        "ATA IO primary: 0x{:04X} / 0x{:04X}",
        ata_cfg.primary_cmd, ata_cfg.primary_ctrl
    ));
    console::write_line(&format!(
        "ATA IO secondary: 0x{:04X} / 0x{:04X}",
        ata_cfg.secondary_cmd, ata_cfg.secondary_ctrl
    ));
    if let Some(pci) = ata_cfg.pci {
        console::write_line(&format!(
            "PCI IDE: {:02X}:{:02X}.{} prog-if 0x{:02X} cmd 0x{:04X}",
            pci.bus, pci.device, pci.function, pci.prog_if, pci.command
        ));
        console::write_line(&format!(
            "PCI BARs: [{:08X} {:08X} {:08X} {:08X}]",
            pci.bar0, pci.bar1, pci.bar2, pci.bar3
        ));
    } else {
        console::write_line("PCI IDE: not detected (legacy ports only).");
    }
}
