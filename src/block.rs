// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

use std::process::Command;

use anyhow::{Context, Result};
use serde::Deserialize;

pub struct Device {
    pub path: String,
    pub model: String,
    pub size: String,
    pub transport: String,
    pub busy: bool,
    pub busy_reason: String,
}

#[derive(Deserialize)]
struct LsblkOutput {
    blockdevices: Vec<LsblkEntry>,
}

#[derive(Deserialize)]
struct LsblkEntry {
    name: Option<String>,
    path: Option<String>,
    #[serde(rename = "type")]
    entry_type: Option<String>,
    size: Option<serde_json::Value>,
    model: Option<String>,
    tran: Option<String>,
    mountpoint: Option<String>,
    children: Option<Vec<LsblkEntry>>,
}

fn human_size(size_b: &serde_json::Value) -> String {
    let bytes: f64 = match size_b {
        serde_json::Value::Number(n) => n.as_f64().unwrap_or(0.0),
        serde_json::Value::String(s) => s.parse().unwrap_or(0.0),
        _ => return "—".into(),
    };
    if bytes == 0.0 {
        return "—".into();
    }
    crate::util::human_size(bytes as u64)
}

fn is_busy(entry: &LsblkEntry) -> (bool, String) {
    fn walk(node: &LsblkEntry, root: &LsblkEntry) -> Option<(String, String)> {
        if let Some(mp) = &node.mountpoint {
            if !mp.is_empty() {
                let ident = node.path.as_deref().or(node.name.as_deref()).unwrap_or("?");
                return Some(("mounted".into(), format!("{ident} → {mp}")));
            }
        }
        let t = node.entry_type.as_deref().unwrap_or("").to_lowercase();
        if !std::ptr::eq(node, root) && matches!(t.as_str(), "crypt" | "raid" | "lvm") {
            let ident = node.path.as_deref().or(node.name.as_deref()).unwrap_or("?");
            return Some((t, ident.to_string()));
        }
        if let Some(children) = &node.children {
            for child in children {
                if let Some(result) = walk(child, root) {
                    return Some(result);
                }
            }
        }
        None
    }

    match walk(entry, entry) {
        Some((kind, what)) => (true, format!("{kind}: {what}")),
        None => (false, String::new()),
    }
}

pub fn list_disks() -> Result<Vec<Device>> {
    let output = Command::new("lsblk")
        .args([
            "-J",
            "-b",
            "-o",
            "NAME,PATH,KNAME,TYPE,SIZE,MODEL,TRAN,MOUNTPOINT",
        ])
        .output()
        .context("failed to run lsblk")?;

    if !output.status.success() {
        anyhow::bail!(
            "lsblk failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let data: LsblkOutput =
        serde_json::from_slice(&output.stdout).context("failed to parse lsblk JSON output")?;

    let mut disks = Vec::new();
    for entry in &data.blockdevices {
        if entry.entry_type.as_deref() != Some("disk") {
            continue;
        }

        let path = entry
            .path
            .clone()
            .unwrap_or_else(|| format!("/dev/{}", entry.name.as_deref().unwrap_or("")));

        let (busy, reason) = is_busy(entry);

        disks.push(Device {
            path,
            model: entry
                .model
                .as_deref()
                .unwrap_or("(unknown)")
                .trim()
                .to_string(),
            size: entry.size.as_ref().map_or_else(|| "—".into(), human_size),
            transport: {
                let t = entry.tran.as_deref().unwrap_or("").to_uppercase();
                if t.is_empty() { "—".to_string() } else { t }
            },
            busy,
            busy_reason: reason,
        });
    }

    Ok(disks)
}
