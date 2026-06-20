// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

//! Minimal DER encoding and decoding helpers (no third-party ASN.1 stack).

use anyhow::{Result, anyhow};

/// Encode a DER definite-length prefix.
pub(crate) fn der_len(len: usize) -> Vec<u8> {
    if len < 0x80 {
        return vec![len as u8];
    }
    let bytes = len.to_be_bytes();
    let start = bytes
        .iter()
        .position(|&b| b != 0)
        .unwrap_or(bytes.len() - 1);
    let body = &bytes[start..];
    let mut out = Vec::with_capacity(1 + body.len());
    out.push(0x80 | body.len() as u8);
    out.extend_from_slice(body);
    out
}

/// Encode one DER TLV: `tag || length || contents`.
pub(crate) fn der_tlv(tag: u8, contents: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + contents.len());
    out.push(tag);
    out.extend_from_slice(&der_len(contents.len()));
    out.extend_from_slice(contents);
    out
}

/// SEQUENCE built from already-encoded elements.
pub(crate) fn der_seq(elems: &[Vec<u8>]) -> Vec<u8> {
    der_tlv(0x30, &elems.concat())
}

/// SET built from already-encoded elements. The caller orders the elements per
/// the DER SET OF rules (or passes a single element).
pub(crate) fn der_set(elems: &[Vec<u8>]) -> Vec<u8> {
    der_tlv(0x31, &elems.concat())
}

pub(crate) fn der_octet(bytes: &[u8]) -> Vec<u8> {
    der_tlv(0x04, bytes)
}

pub(crate) fn der_int_u8(value: u8) -> Vec<u8> {
    der_tlv(0x02, &[value])
}

pub(crate) fn der_null() -> Vec<u8> {
    vec![0x05, 0x00]
}

/// Encode an OBJECT IDENTIFIER from its arc values.
pub(crate) fn der_oid(arcs: &[u32]) -> Vec<u8> {
    let mut body = vec![(arcs[0] * 40 + arcs[1]) as u8];
    for &arc in &arcs[2..] {
        let mut group = vec![(arc & 0x7f) as u8];
        let mut rest = arc >> 7;
        while rest > 0 {
            group.push(0x80 | (rest & 0x7f) as u8);
            rest >>= 7;
        }
        group.reverse();
        body.extend_from_slice(&group);
    }
    der_tlv(0x06, &body)
}

/// `AlgorithmIdentifier { algorithm, NULL }`.
pub(crate) fn algorithm_identifier(oid: &[u32]) -> Vec<u8> {
    der_seq(&[der_oid(oid), der_null()])
}

/// A single DER element: its tag, its contents, and the full `tag||len||value`
/// slice it occupies.
pub(crate) struct Tlv<'a> {
    pub(crate) tag: u8,
    pub(crate) contents: &'a [u8],
    pub(crate) total: &'a [u8],
}

/// Read the TLV at `data[pos..]`, returning it and the offset just past it.
pub(crate) fn der_read(data: &[u8], pos: usize) -> Result<(Tlv<'_>, usize)> {
    let tag = *data.get(pos).ok_or_else(|| anyhow!("truncated DER tag"))?;
    let first = *data
        .get(pos + 1)
        .ok_or_else(|| anyhow!("truncated DER length"))?;
    let (header, len) = if first < 0x80 {
        (2, first as usize)
    } else {
        let n = (first & 0x7f) as usize;
        let mut len = 0usize;
        for i in 0..n {
            let byte = *data
                .get(pos + 2 + i)
                .ok_or_else(|| anyhow!("truncated DER length"))?;
            len = (len << 8) | byte as usize;
        }
        (2 + n, len)
    };
    let start = pos + header;
    let end = start
        .checked_add(len)
        .filter(|&e| e <= data.len())
        .ok_or_else(|| anyhow!("DER element exceeds buffer"))?;
    Ok((
        Tlv {
            tag,
            contents: &data[start..end],
            total: &data[pos..end],
        },
        end,
    ))
}
