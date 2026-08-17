// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

//! GPT partition-table creation and inspection, plus the derived partition
//! geometry the install pipeline operates on.

use std::ffi::OsString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use uuid::Uuid;

use crate::engine::config::PartitionSize;
use crate::engine::util::random_bytes;

const PART_SIZE_UNITS: &[(&str, u64)] = &[
    ("K", 1024),
    ("M", 1024 * 1024),
    ("G", 1024 * 1024 * 1024),
    ("T", 1024 * 1024 * 1024 * 1024),
    ("P", 1024 * 1024 * 1024 * 1024 * 1024),
];

pub(crate) fn part_size_bytes(size: &str) -> Result<u64> {
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
    drive: impl AsRef<Path>,
    efi_size: &PartitionSize,
    home_size: &PartitionSize,
) -> Result<PartitionLayout> {
    let drive = drive.as_ref();
    let efi_bytes = efi_size.bytes();
    let home_bytes = home_size.bytes();

    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_EXCL)
        .open(drive)
        .with_context(|| format!("failed to open {} for exclusive access", drive.display()))?;
    let sector_size = gptman::linux::get_sector_size(&mut file)
        .with_context(|| format!("failed to determine sector size of {}", drive.display()))?;

    let disk_guid = random_bytes::<16>()?;
    let mut gpt = gptman::GPT::new_from(&mut file, sector_size, disk_guid)?;

    let efi_sectors = efi_bytes.div_ceil(sector_size);
    let home_sectors = home_bytes.div_ceil(sector_size);
    if efi_sectors == 0 || home_sectors == 0 {
        bail!("partition sizes must be at least one sector");
    }

    let start = gpt
        .find_first_place(efi_sectors)
        .ok_or_else(|| anyhow::anyhow!("no room for EFI partition on {}", drive.display()))?;
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
        .ok_or_else(|| anyhow::anyhow!("no room for home partition on {}", drive.display()))?;
    let end = gpt.header.last_usable_lba;
    set_entry(&mut gpt, 3, PART_TYPE_LINUX, start, end, "home")?;
    let size = gpt.get_maximum_partition_size()?;
    let start = gpt
        .find_optimal_place(size)
        .ok_or_else(|| anyhow::anyhow!("no room for root partition on {}", drive.display()))?;
    set_entry(
        &mut gpt,
        2,
        PART_TYPE_LINUX,
        start,
        start + size - 1,
        "root",
    )?;

    let part_bytes = |n: u32| (gpt[n].ending_lba - gpt[n].starting_lba + 1) * sector_size;
    tracing::info!(
        target: crate::engine::INSTALLATION_LOG_TARGET,
        "wrote GPT to {}: EFI {}, root {}, home {}",
        drive.display(),
        crate::engine::util::human_size(part_bytes(1)),
        crate::engine::util::human_size(part_bytes(2)),
        crate::engine::util::human_size(part_bytes(3)),
    );

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
    drive: &Path,
    sector_size: u64,
) -> Result<()> {
    gptman::GPT::write_protective_mbr_into(file, sector_size)?;
    gpt.write_into(file)?;
    file.sync_all()?;
    gptman::linux::reread_partition_table(file).map_err(|e| {
        anyhow::anyhow!(
            "failed to reread partition table on {}: {e}",
            drive.display()
        )
    })
}

/// Read the EFI partition layout metadata from an existing GPT on `drive`.
pub fn load_partition_layout(drive: impl AsRef<Path>) -> Result<PartitionLayout> {
    let drive = drive.as_ref();
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .open(drive)
        .map_err(|e| anyhow::anyhow!("failed to open {}: {e}", drive.display()))?;
    let gpt = gptman::GPT::find_from(&mut file)
        .map_err(|e| anyhow::anyhow!("failed to read GPT from {}: {e}", drive.display()))?;

    for number in 1..=gpt.header.number_of_partition_entries {
        let entry = &gpt[number];
        if entry.is_used() && entry.partition_type_guid == PART_TYPE_ESP {
            return Ok(PartitionLayout {
                efi: PartitionInfo::from_entry(number, entry),
            });
        }
    }

    Err(anyhow::anyhow!(
        "no EFI partition found on {}",
        drive.display()
    ))
}

/// Return the minimum target disk size required by the configured partition sizes.
pub fn min_target_size_bytes(config: &crate::engine::config::Config) -> Result<u64> {
    let padding = "1G".parse::<PartitionSize>()?.bytes();
    config
        .partitions
        .efi
        .bytes()
        .checked_add(config.partitions.root_min.bytes())
        .and_then(|total| total.checked_add(config.partitions.home.bytes()))
        .and_then(|total| total.checked_add(padding))
        .ok_or_else(|| anyhow::anyhow!("minimum target size is too large"))
}

pub fn derive_partitions(drive: impl AsRef<Path>) -> DerivedPartitions {
    let drive = drive.as_ref();
    DerivedPartitions {
        efi_part: partition_path(drive, 1),
        root_part: partition_path(drive, 2),
        home_part: partition_path(drive, 3),
        ..Default::default()
    }
}

fn partition_path(drive: &Path, number: u8) -> PathBuf {
    let mut name = OsString::from(drive.as_os_str());
    if drive
        .as_os_str()
        .as_bytes()
        .last()
        .is_some_and(u8::is_ascii_digit)
    {
        name.push("p");
    }
    name.push(number.to_string());
    PathBuf::from(name)
}

#[derive(Debug, Clone, Default)]
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

#[derive(Debug, Clone, Default)]
pub struct PartitionLayout {
    pub efi: PartitionInfo,
}

#[derive(Debug, Clone, Default)]
pub struct DerivedPartitions {
    pub efi_part: PathBuf,
    pub efi_uuid: Option<Uuid>,
    pub root_part: PathBuf,
    pub root_uuid: Option<Uuid>,
    pub home_part: PathBuf,
    pub home_uuid: Option<Uuid>,
    pub layout: Option<PartitionLayout>,
}
