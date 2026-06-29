// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

//! GPT partition-table creation and inspection, plus the derived partition
//! geometry the install pipeline operates on.

use anyhow::Result;

use crate::engine::util::random_bytes;

const PART_SIZE_UNITS: &[(&str, u64)] = &[
    ("K", 1024),
    ("M", 1024 * 1024),
    ("G", 1024 * 1024 * 1024),
    ("T", 1024 * 1024 * 1024 * 1024),
    ("P", 1024 * 1024 * 1024 * 1024 * 1024),
];

fn part_size_bytes(size: &str) -> Result<u64> {
    let s = size.strip_prefix('+').unwrap_or(size);
    let (num_str, unit) = s
        .split_at_checked(s.len().saturating_sub(1))
        .ok_or_else(|| anyhow::anyhow!("unsupported partition size: {size}"))?;
    let num: u64 = num_str
        .parse()
        .map_err(|_| anyhow::anyhow!("unsupported partition size: {size}"))?;
    let multiplier = PART_SIZE_UNITS
        .iter()
        .find(|(u, _)| *u == unit)
        .map(|(_, m)| *m)
        .ok_or_else(|| anyhow::anyhow!("unsupported partition unit: {unit}"))?;
    num.checked_mul(multiplier)
        .ok_or_else(|| anyhow::anyhow!("partition size is too large: {size}"))
}

// GPT partition type GUIDs, in on-disk (mixed-endian) byte order.
// EFI System Partition: C12A7328-F81F-11D2-BA4B-00A0C93EC93B
const PART_TYPE_ESP: [u8; 16] = [
    0x28, 0x73, 0x2A, 0xC1, 0x1F, 0xF8, 0xD2, 0x11, 0xBA, 0x4B, 0x00, 0xA0, 0xC9, 0x3E, 0xC9, 0x3B,
];
// Linux filesystem: 0FC63DAF-8483-4772-8E79-3D69D8477DE4
const PART_TYPE_LINUX: [u8; 16] = [
    0xAF, 0x3D, 0xC6, 0x0F, 0x83, 0x84, 0x72, 0x47, 0x8E, 0x79, 0x3D, 0x69, 0xD8, 0x47, 0x7D, 0xE4,
];

/// Create a fresh GPT on `drive` with EFI, root and home partitions, using the
/// `gptman` crate instead of shelling out to `sgdisk`/`partprobe`.
///
/// Layout mirrors the previous `sgdisk` calls: EFI (`efi_size`) at the front,
/// home (`home_size`) pinned to the end of the disk, and root filling the gap.
pub fn partition_drive(
    log: &mut dyn FnMut(String),
    drive: &str,
    efi_size: &str,
    home_size: &str,
) -> Result<PartitionLayout> {
    let efi_bytes = part_size_bytes(efi_size)?;
    let home_bytes = part_size_bytes(home_size)?;

    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(drive)?;
    let sector_size = gptman::linux::get_sector_size(&mut file).unwrap_or(512);

    let disk_guid = random_bytes::<16>()?;
    let mut gpt = gptman::GPT::new_from(&mut file, sector_size, disk_guid)?;

    let efi_sectors = efi_bytes.div_ceil(sector_size);
    let home_sectors = home_bytes.div_ceil(sector_size);

    let start = gpt
        .find_first_place(efi_sectors)
        .ok_or_else(|| anyhow::anyhow!("no room for EFI partition on {drive}"))?;
    set_entry(
        &mut gpt,
        1,
        PART_TYPE_ESP,
        start,
        start + efi_sectors - 1,
        "EFI",
    )?;
    let start = gpt
        .find_last_place(home_sectors)
        .ok_or_else(|| anyhow::anyhow!("no room for home partition on {drive}"))?;
    let end = gpt.header.last_usable_lba;
    set_entry(&mut gpt, 3, PART_TYPE_LINUX, start, end, "home")?;
    let size = gpt.get_maximum_partition_size()?;
    let start = gpt
        .find_optimal_place(size)
        .ok_or_else(|| anyhow::anyhow!("no room for root partition on {drive}"))?;
    set_entry(
        &mut gpt,
        2,
        PART_TYPE_LINUX,
        start,
        start + size - 1,
        "root",
    )?;

    let part_bytes = |n: u32| (gpt[n].ending_lba - gpt[n].starting_lba + 1) * sector_size;
    log(format!(
        "wrote GPT to {drive}: EFI {}, root {}, home {}",
        crate::engine::util::human_size(part_bytes(1)),
        crate::engine::util::human_size(part_bytes(2)),
        crate::engine::util::human_size(part_bytes(3)),
    ));

    commit_gpt(&mut file, &mut gpt, drive, sector_size)?;

    Ok(PartitionLayout {
        efi: PartitionInfo::from_entry(1, &gpt[1]),
    })
}

fn set_entry(
    gpt: &mut gptman::GPT,
    index: u32,
    partition_type_guid: [u8; 16],
    start: u64,
    end: u64,
    name: &str,
) -> Result<()> {
    gpt[index] = gptman::GPTPartitionEntry {
        partition_type_guid,
        unique_partition_guid: random_bytes::<16>()?,
        starting_lba: start,
        ending_lba: end,
        attribute_bits: 0,
        partition_name: name.into(),
    };
    Ok(())
}

fn commit_gpt(
    file: &mut std::fs::File,
    gpt: &mut gptman::GPT,
    drive: &str,
    sector_size: u64,
) -> Result<()> {
    gptman::GPT::write_protective_mbr_into(file, sector_size)?;
    gpt.write_into(file)?;
    file.sync_all()?;
    gptman::linux::reread_partition_table(file)
        .map_err(|e| anyhow::anyhow!("failed to reread partition table on {drive}: {e}"))
}

/// Read the EFI partition layout metadata from an existing GPT on `drive`.
pub fn load_partition_layout(drive: &str) -> Result<PartitionLayout> {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .open(drive)
        .map_err(|e| anyhow::anyhow!("failed to open {drive}: {e}"))?;
    let gpt = gptman::GPT::find_from(&mut file)
        .map_err(|e| anyhow::anyhow!("failed to read GPT from {drive}: {e}"))?;

    for number in 1..=gpt.header.number_of_partition_entries {
        let entry = &gpt[number];
        if entry.is_used() && entry.partition_type_guid == PART_TYPE_ESP {
            return Ok(PartitionLayout {
                efi: PartitionInfo::from_entry(number, entry),
            });
        }
    }

    Err(anyhow::anyhow!("no EFI partition found on {drive}"))
}

/// Return the minimum target disk size required by the configured partition sizes.
pub fn min_target_size_bytes(config: &crate::engine::config::Config) -> Result<u64> {
    let efi = format!("+{}", config.partitions.efi);
    let root = format!("+{}", config.partitions.root_min);
    let home = format!("+{}", config.partitions.home);
    let sizes = [efi.as_str(), root.as_str(), home.as_str(), "+1G"];

    sizes.iter().try_fold(0u64, |total, size| {
        let bytes = part_size_bytes(size)?;
        total
            .checked_add(bytes)
            .ok_or_else(|| anyhow::anyhow!("minimum target size is too large"))
    })
}

pub fn derive_partitions(drive: &str) -> DerivedPartitions {
    let sep = if drive.ends_with(|c: char| c.is_ascii_digit()) {
        "p"
    } else {
        ""
    };
    DerivedPartitions {
        efi_part: format!("{drive}{sep}1"),
        root_part: format!("{drive}{sep}2"),
        home_part: format!("{drive}{sep}3"),
        ..Default::default()
    }
}

#[derive(Debug, Clone)]
pub struct PartitionInfo {
    pub number: u32,
    pub unique_guid: [u8; 16],
    pub starting_lba: u64,
    pub ending_lba: u64,
}

impl PartitionInfo {
    fn from_entry(number: u32, entry: &gptman::GPTPartitionEntry) -> Self {
        Self {
            number,
            unique_guid: entry.unique_partition_guid,
            starting_lba: entry.starting_lba,
            ending_lba: entry.ending_lba,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PartitionLayout {
    pub efi: PartitionInfo,
}

#[derive(Debug, Clone, Default)]
pub struct DerivedPartitions {
    pub efi_part: String,
    pub efi_uuid: String,
    pub root_part: String,
    pub root_uuid: String,
    pub home_part: String,
    pub home_uuid: String,
    pub layout: Option<PartitionLayout>,
}
