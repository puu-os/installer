// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

//! Native Secure Boot signing and key enrollment, replacing the `sbctl`
//! `sign`/`verify`/`enroll-keys` subcommands.
//!
//! All cryptography goes through OpenSSL (libcrypto) so the binary depends on a
//! single, FIPS-certifiable crypto backend. The Authenticode/PKCS#7 and PE
//! container structures are assembled and parsed with the hand-rolled DER
//! primitives in [`crate::engine::der`] rather than a third-party ASN.1 stack.

#![allow(clippy::doc_markdown)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};
use openssl::hash::MessageDigest;
use openssl::pkcs7::{Pkcs7, Pkcs7Flags};
use openssl::pkey::{PKey, PKeyRef, Private};
use openssl::sha::{Sha256, sha256};
use openssl::sign::Signer;
use openssl::stack::Stack;
use openssl::x509::X509;
use openssl::x509::store::X509StoreBuilder;

use crate::engine::der::{
    algorithm_identifier, der_int_u8, der_octet, der_oid, der_read, der_seq, der_set, der_tlv,
};
use crate::engine::pipeline::{TARGET_EFI_MOUNT, TARGET_MOUNT};

// Object identifiers, expressed as arc sequences and DER-encoded on demand.
const OID_SPC_INDIRECT_DATA: &[u32] = &[1, 3, 6, 1, 4, 1, 311, 2, 1, 4];
const OID_SPC_PE_IMAGE_DATA: &[u32] = &[1, 3, 6, 1, 4, 1, 311, 2, 1, 15];
const OID_SHA256: &[u32] = &[2, 16, 840, 1, 101, 3, 4, 2, 1];
const OID_RSA_ENCRYPTION: &[u32] = &[1, 2, 840, 113_549, 1, 1, 1];
const OID_SIGNED_DATA: &[u32] = &[1, 2, 840, 113_549, 1, 7, 2];
const OID_CONTENT_TYPE: &[u32] = &[1, 2, 840, 113_549, 1, 9, 3];
const OID_MESSAGE_DIGEST: &[u32] = &[1, 2, 840, 113_549, 1, 9, 4];

// WIN_CERTIFICATE wrapper constants.
const WIN_CERT_REVISION_2_0: u16 = 0x0200;
const WIN_CERT_TYPE_PKCS_SIGNED_DATA: u16 = 0x0002;

// Well-known "obsolete" SpcPeImageData value (as emitted by signtool/sbsign):
// SEQUENCE { flags BIT STRING {}, file [0] SpcLink:[2] { [0] BMPString "<<<Obsolete>>>" } }
const SPC_PE_IMAGE_DATA_VALUE: &[u8] = &[
    0x30, 0x25, // SEQUENCE (37 bytes)
    0x03, 0x01, 0x00, // flags BIT STRING {}
    0xa0, 0x20, // file [0] (32 bytes)
    0xa2, 0x1e, // SpcLink file [2] (30 bytes)
    0x80, 0x1c, // SpcString unicode [0] BMPString (28 bytes)
    0x00, 0x3c, 0x00, 0x3c, 0x00, 0x3c, 0x00, 0x4f, 0x00, 0x62, 0x00, 0x73, 0x00, 0x6f, 0x00, 0x6c,
    0x00, 0x65, 0x00, 0x74, 0x00, 0x65, 0x00, 0x3e, 0x00, 0x3e, 0x00, 0x3e,
];

/// Extract the raw `issuer` Name and `serialNumber` TLVs from a certificate's
/// DER, for use in a CMS `IssuerAndSerialNumber`.
fn issuer_and_serial(cert_der: &[u8]) -> Result<(Vec<u8>, Vec<u8>)> {
    let (cert, _) = der_read(cert_der, 0)?;
    if cert.tag != 0x30 {
        bail!("certificate is not a SEQUENCE");
    }
    let (tbs, _) = der_read(cert.contents, 0)?;
    if tbs.tag != 0x30 {
        bail!("tbsCertificate is not a SEQUENCE");
    }
    let body = tbs.contents;
    let (first, mut pos) = der_read(body, 0)?;
    // Skip the optional [0] EXPLICIT version.
    if first.tag != 0xa0 {
        pos = 0;
    }
    let (serial, after_serial) = der_read(body, pos)?;
    if serial.tag != 0x02 {
        bail!("serialNumber is not an INTEGER");
    }
    let (_signature, after_sig) = der_read(body, after_serial)?;
    let (issuer, _) = der_read(body, after_sig)?;
    if issuer.tag != 0x30 {
        bail!("issuer is not a Name SEQUENCE");
    }
    Ok((issuer.total.to_vec(), serial.total.to_vec()))
}

/// Parse the embedded Authenticode digest and signer certificates out of a
/// PKCS#7 `SignedData` ContentInfo. Returns `None` if the structure does not
/// match the expected Authenticode shape.
fn parse_authenticode(pkcs7: &[u8]) -> Option<(Vec<u8>, Vec<Vec<u8>>)> {
    let (content_info, _) = der_read(pkcs7, 0).ok()?;
    if content_info.tag != 0x30 {
        return None;
    }
    let (_content_type, after_type) = der_read(content_info.contents, 0).ok()?;
    let (content, _) = der_read(content_info.contents, after_type).ok()?;
    if content.tag != 0xa0 {
        return None;
    }
    let (signed_data, _) = der_read(content.contents, 0).ok()?;
    if signed_data.tag != 0x30 {
        return None;
    }
    let body = signed_data.contents;

    // version, digestAlgorithms, encapContentInfo, then optional [0] certs /
    // [1] crls and the signerInfos SET.
    let (_version, mut pos) = der_read(body, 0).ok()?;
    let (_digest_algs, next) = der_read(body, pos).ok()?;
    pos = next;
    let (encap, next) = der_read(body, pos).ok()?;
    pos = next;

    // encapContentInfo: eContentType OID, eContent [0] EXPLICIT.
    let (_econtent_type, e1) = der_read(encap.contents, 0).ok()?;
    let (econtent, _) = der_read(encap.contents, e1).ok()?;
    if econtent.tag != 0xa0 {
        return None;
    }
    // SpcIndirectDataContent: data SpcAttr, message_digest DigestInfo.
    let (spc, _) = der_read(econtent.contents, 0).ok()?;
    let (_spc_data, s1) = der_read(spc.contents, 0).ok()?;
    let (digest_info, _) = der_read(spc.contents, s1).ok()?;
    // DigestInfo: AlgorithmIdentifier, OCTET STRING.
    let (_alg, d1) = der_read(digest_info.contents, 0).ok()?;
    let (digest, _) = der_read(digest_info.contents, d1).ok()?;
    if digest.tag != 0x04 {
        return None;
    }

    // Remaining top-level elements: the [0] IMPLICIT certificate set holds the
    // concatenated signer certificates.
    let mut certs = Vec::new();
    while pos < body.len() {
        let (elem, next) = der_read(body, pos).ok()?;
        pos = next;
        if elem.tag == 0xa0 {
            let mut cpos = 0;
            while cpos < elem.contents.len() {
                let (cert, cnext) = der_read(elem.contents, cpos).ok()?;
                certs.push(cert.total.to_vec());
                cpos = cnext;
            }
        }
    }

    Some((digest.contents.to_vec(), certs))
}

/// Parse a PEM private key and certificate pair via OpenSSL.
fn load_key_and_cert(key_pem: &str, cert_pem: &str) -> Result<(PKey<Private>, X509)> {
    let key = PKey::private_key_from_pem(key_pem.as_bytes())?;
    let cert = X509::from_pem(cert_pem.as_bytes())?;
    Ok((key, cert))
}

/// Build the Authenticode PKCS#7 SignedData blob for a precomputed PE digest.
/// The ASN.1 is hand-encoded; the RSA-SHA256 signature over the signed
/// attributes is produced by OpenSSL.
fn build_pkcs7(digest: &[u8], key: &PKeyRef<Private>, cert: &X509) -> Result<Vec<u8>> {
    let cert_der = cert.to_der()?;
    let (issuer, serial) = issuer_and_serial(&cert_der)?;

    // SpcIndirectDataContent { SpcAttributeTypeAndOptionalValue, DigestInfo }.
    let spc_attr = der_seq(&[
        der_oid(OID_SPC_PE_IMAGE_DATA),
        SPC_PE_IMAGE_DATA_VALUE.to_vec(),
    ]);
    let spc_digest = der_seq(&[algorithm_identifier(OID_SHA256), der_octet(digest)]);
    let spc = der_seq(&[spc_attr, spc_digest]);

    // Authenticode signs the SpcIndirectDataContent SEQUENCE contents, not
    // its outer DER tag and length.
    let (spc_tlv, spc_end) = der_read(&spc, 0)?;
    if spc_tlv.tag != 0x30 || spc_end != spc.len() {
        bail!("invalid SpcIndirectDataContent encoding");
    }
    let content_digest = sha256(spc_tlv.contents);
    let ct_attr = der_seq(&[
        der_oid(OID_CONTENT_TYPE),
        der_set(&[der_oid(OID_SPC_INDIRECT_DATA)]),
    ]);
    let md_attr = der_seq(&[
        der_oid(OID_MESSAGE_DIGEST),
        der_set(&[der_octet(&content_digest)]),
    ]);
    // DER SET OF requires the elements to be sorted by their encoding.
    let mut attr_elems = [ct_attr, md_attr];
    attr_elems.sort_unstable();
    let attrs_contents = attr_elems.concat();

    // RSA-SHA256 signature over the DER SET OF signed attributes (tag 0x31).
    let signed_attrs_to_sign = der_tlv(0x31, &attrs_contents);
    let mut signer = Signer::new(MessageDigest::sha256(), key)?;
    signer.update(&signed_attrs_to_sign)?;
    let signature = signer.sign_to_vec()?;

    // SignerInfo. signedAttrs appears as [0] IMPLICIT inside the structure.
    let issuer_and_serial = der_seq(&[issuer, serial]);
    let signed_attrs_implicit = der_tlv(0xa0, &attrs_contents);
    let signer_info = der_seq(&[
        der_int_u8(1), // version
        issuer_and_serial,
        algorithm_identifier(OID_SHA256),
        signed_attrs_implicit,
        algorithm_identifier(OID_RSA_ENCRYPTION),
        der_octet(&signature),
    ]);

    // SignedData.
    let econtent = der_tlv(0xa0, &spc); // [0] EXPLICIT SpcIndirectDataContent
    let encap = der_seq(&[der_oid(OID_SPC_INDIRECT_DATA), econtent]);
    let certificates = der_tlv(0xa0, &cert_der); // [0] IMPLICIT SET OF Certificate
    let signed_data = der_seq(&[
        der_int_u8(1), // version
        der_set(&[algorithm_identifier(OID_SHA256)]),
        encap,
        certificates,
        der_set(&[signer_info]),
    ]);

    // ContentInfo { signedData, [0] EXPLICIT SignedData }.
    let content = der_tlv(0xa0, &signed_data);
    Ok(der_seq(&[der_oid(OID_SIGNED_DATA), content]))
}

fn read_u32_le(data: &[u8], off: usize) -> Result<u32> {
    let bytes: [u8; 4] = data
        .get(off..off + 4)
        .ok_or_else(|| anyhow!("truncated PE at offset {off:#x}"))?
        .try_into()
        .map_err(|_| anyhow!("slice length mismatch reading PE"))?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u16_le(data: &[u8], off: usize) -> Result<u16> {
    let bytes: [u8; 2] = data
        .get(off..off + 2)
        .ok_or_else(|| anyhow!("truncated PE at offset {off:#x}"))?
        .try_into()
        .map_err(|_| anyhow!("slice length mismatch reading PE"))?;
    Ok(u16::from_le_bytes(bytes))
}

/// Offset of the PE optional header: PE signature (4) + COFF file header (20)
/// after `e_lfanew`.
fn optional_header_offset(data: &[u8]) -> Result<usize> {
    Ok(read_u32_le(data, 0x3c)? as usize + 24)
}

/// Offset of the IMAGE_DATA_DIRECTORY for the certificate table (index 4)
/// within the PE optional header.
fn security_dir_offset(data: &[u8]) -> Result<usize> {
    let opt = optional_header_offset(data)?;
    let magic = read_u16_le(data, opt)?;
    // NumberOfRvaAndSizes lives at a magic-dependent offset; data directories
    // start right after it. PE32: dirs at opt+96; PE32+: opt+112.
    let dir_start = match magic {
        0x10b => opt + 96,  // PE32
        0x20b => opt + 112, // PE32+
        other => bail!("unsupported PE optional header magic {other:#x}"),
    };
    Ok(dir_start + 4 * 8) // certificate table = directory entry 4
}

/// Return a validated terminal attribute-certificate table range.
fn certificate_table_range(image: &[u8]) -> Result<Option<std::ops::Range<usize>>> {
    let sec_dir = security_dir_offset(image)?;
    let table_off = read_u32_le(image, sec_dir)? as usize;
    let table_size = read_u32_le(image, sec_dir + 4)? as usize;
    if table_off == 0 && table_size == 0 {
        return Ok(None);
    }
    if table_off == 0 || table_size == 0 {
        bail!("incomplete PE certificate table directory");
    }
    if !table_off.is_multiple_of(8) {
        bail!("PE certificate table is not 8-byte aligned");
    }
    let table_end = table_off
        .checked_add(table_size)
        .ok_or_else(|| anyhow!("PE certificate table range overflows"))?;
    if table_end != image.len() {
        bail!("PE certificate table must be the final image region");
    }

    let opt = optional_header_offset(image)?;
    let size_of_headers = read_u32_le(image, opt + 60)? as usize;
    let e_lfanew = read_u32_le(image, 0x3c)? as usize;
    let num_sections = read_u16_le(image, e_lfanew + 6)? as usize;
    let size_of_optional_header = read_u16_le(image, e_lfanew + 20)? as usize;
    let section_table = opt
        .checked_add(size_of_optional_header)
        .ok_or_else(|| anyhow!("PE section table offset overflows"))?;
    let mut image_data_end = size_of_headers;
    for index in 0..num_sections {
        let header = section_table
            .checked_add(
                index
                    .checked_mul(40)
                    .ok_or_else(|| anyhow!("PE section index overflows"))?,
            )
            .ok_or_else(|| anyhow!("PE section header offset overflows"))?;
        let start = read_u32_le(image, header + 20)? as usize;
        let size = read_u32_le(image, header + 16)? as usize;
        let end = start
            .checked_add(size)
            .ok_or_else(|| anyhow!("PE section range overflows"))?;
        if end > image.len() {
            bail!("PE section exceeds image");
        }
        image_data_end = image_data_end.max(end);
    }
    if table_off < image_data_end {
        bail!("PE certificate table overlaps headers or section data");
    }
    Ok(Some(table_off..table_end))
}

/// Compute the Authenticode SHA-256 digest of a PE image, hashing everything
/// except the optional-header checksum, the certificate-table data directory
/// entry, and the attribute certificate table itself.
fn authenticode_sha256(image: &[u8]) -> Result<[u8; 32]> {
    let e_lfanew = read_u32_le(image, 0x3c)? as usize;
    let opt = optional_header_offset(image)?;
    let check_sum = opt + 64;
    let after_check_sum = check_sum + 4;
    let sec_dir = security_dir_offset(image)?;
    let after_sec_dir = sec_dir + 8;
    let size_of_headers = read_u32_le(image, opt + 60)? as usize;

    let slice = |start: usize, end: usize| -> Result<&[u8]> {
        image
            .get(start..end)
            .ok_or_else(|| anyhow!("PE region {start:#x}..{end:#x} out of bounds"))
    };

    let mut hasher = Sha256::new();
    hasher.update(slice(0, check_sum)?);
    hasher.update(slice(after_check_sum, sec_dir)?);
    hasher.update(slice(after_sec_dir, size_of_headers)?);

    // Section raw data, in ascending PointerToRawData order.
    let num_sections = read_u16_le(image, e_lfanew + 6)? as usize;
    let size_of_optional_header = read_u16_le(image, e_lfanew + 20)? as usize;
    let section_table = opt + size_of_optional_header;
    let mut ranges = Vec::with_capacity(num_sections);
    for i in 0..num_sections {
        let header = section_table + i * 40;
        let ptr = read_u32_le(image, header + 20)? as usize;
        let size = read_u32_le(image, header + 16)? as usize;
        ranges.push((ptr, ptr + size));
    }
    ranges.sort_unstable_by_key(|r| r.0);

    let mut bytes_hashed = size_of_headers;
    for (start, end) in ranges {
        hasher.update(slice(start, end)?);
        bytes_hashed += end - start;
    }

    // Trailing data after the sections, minus the attribute certificate table.
    let cert_table_size = read_u32_le(image, sec_dir + 4)? as usize;
    let extra = image
        .len()
        .checked_sub(bytes_hashed)
        .and_then(|n| n.checked_sub(cert_table_size))
        .ok_or_else(|| anyhow!("invalid PE layout while hashing trailing data"))?;
    hasher.update(slice(bytes_hashed, bytes_hashed + extra)?);

    Ok(hasher.finish())
}

/// Iterate the PKCS#7 entries of the PE attribute certificate table.
fn attribute_certificates(image: &[u8]) -> Result<Vec<&[u8]>> {
    let Some(table) = certificate_table_range(image)? else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    let mut pos = table.start;
    while pos < table.end {
        let header_end = pos
            .checked_add(8)
            .ok_or_else(|| anyhow!("certificate entry header overflows"))?;
        if header_end > table.end {
            bail!("truncated certificate entry header");
        }
        let cert_size = read_u32_le(image, pos)? as usize;
        let revision = read_u16_le(image, pos + 4)?;
        let cert_type = read_u16_le(image, pos + 6)?;
        if cert_size < 8 {
            bail!("invalid certificate entry size {cert_size}");
        }
        let cert_end = pos
            .checked_add(cert_size)
            .ok_or_else(|| anyhow!("certificate entry range overflows"))?;
        if cert_end > table.end {
            bail!("certificate entry exceeds table");
        }
        if revision == WIN_CERT_REVISION_2_0 && cert_type == WIN_CERT_TYPE_PKCS_SIGNED_DATA {
            out.push(&image[header_end..cert_end]);
        }
        let padded_size = cert_size
            .checked_add(7)
            .ok_or_else(|| anyhow!("certificate entry alignment overflows"))?
            & !7;
        pos = pos
            .checked_add(padded_size)
            .ok_or_else(|| anyhow!("certificate entry offset overflows"))?;
        if pos > table.end {
            bail!("certificate entry padding exceeds table");
        }
    }
    Ok(out)
}

/// Authenticode-sign the PE image bytes, returning the signed image.
pub fn sign_pe(image: &[u8], key_pem: &str, cert_pem: &str) -> Result<Vec<u8>> {
    let (key, cert) = load_key_and_cert(key_pem, cert_pem)?;
    if !cert.public_key()?.public_eq(&key) {
        bail!("Secure Boot private key does not match its certificate");
    }

    let dir = security_dir_offset(image)?;
    let mut out = image.to_vec();
    if let Some(table) = certificate_table_range(image)? {
        out.truncate(table.start);
    }
    out[dir..dir + 8].fill(0);
    while !out.len().is_multiple_of(8) {
        out.push(0);
    }

    let digest = authenticode_sha256(&out)?;
    let pkcs7 = build_pkcs7(&digest, &key, &cert)?;

    // WIN_CERTIFICATE: dwLength (incl. 8-byte header) | wRevision | wType | data,
    // padded so the whole entry is 8-byte aligned.
    let cert_len = 8usize
        .checked_add(pkcs7.len())
        .ok_or_else(|| anyhow!("certificate entry size overflows"))?;
    let cert_len_u32 = u32::try_from(cert_len)
        .map_err(|_| anyhow!("certificate entry is too large for a PE image"))?;
    let padded_len = cert_len
        .checked_add(7)
        .ok_or_else(|| anyhow!("certificate entry alignment overflows"))?
        & !7;
    let mut entry = Vec::with_capacity(padded_len);
    entry.extend_from_slice(&cert_len_u32.to_le_bytes());
    entry.extend_from_slice(&WIN_CERT_REVISION_2_0.to_le_bytes());
    entry.extend_from_slice(&WIN_CERT_TYPE_PKCS_SIGNED_DATA.to_le_bytes());
    entry.extend_from_slice(&pkcs7);
    entry.resize(padded_len, 0);

    let cert_offset = out.len();
    let cert_offset_u32 = u32::try_from(cert_offset)
        .map_err(|_| anyhow!("PE image is too large for a certificate table"))?;
    let table_size_u32 =
        u32::try_from(entry.len()).map_err(|_| anyhow!("PE certificate table is too large"))?;
    out.extend_from_slice(&entry);
    out[dir..dir + 4].copy_from_slice(&cert_offset_u32.to_le_bytes());
    out[dir + 4..dir + 8].copy_from_slice(&table_size_u32.to_le_bytes());

    Ok(out)
}

/// Verify that the PE image carries a cryptographically valid Authenticode
/// signature whose digest matches the image and whose signer is `cert_pem`.
pub fn verify_pe(image: &[u8], cert_pem: &str) -> Result<bool> {
    let Ok(cert) = X509::from_pem(cert_pem.as_bytes()) else {
        return Ok(false);
    };
    let cert_der = cert.to_der()?;
    let expected = authenticode_sha256(image)?;
    let mut certs = Stack::new()?;
    certs.push(cert)?;
    let store = X509StoreBuilder::new()?.build();
    let flags = Pkcs7Flags::BINARY | Pkcs7Flags::NOINTERN | Pkcs7Flags::NOVERIFY;

    for pkcs7_der in attribute_certificates(image)? {
        let Some((digest, embedded_certs)) = parse_authenticode(pkcs7_der) else {
            continue;
        };
        if digest != expected || !embedded_certs.iter().any(|item| item == &cert_der) {
            continue;
        }
        let Ok(pkcs7) = Pkcs7::from_der(pkcs7_der) else {
            continue;
        };
        if pkcs7.verify(&certs, &store, None, None, flags).is_ok() {
            return Ok(true);
        }
    }
    Ok(false)
}

// --- Key enrollment (replaces `sbctl enroll-keys`) ---

// GUIDs in on-disk (mixed-endian) byte order.
const EFI_CERT_X509_GUID: [u8; 16] = [
    0xa1, 0x59, 0xc0, 0xa5, 0xe4, 0x94, 0xa7, 0x4a, 0x87, 0xb5, 0xab, 0x15, 0x5c, 0x2b, 0xf0, 0x72,
];
const EFI_CERT_TYPE_PKCS7_GUID: [u8; 16] = [
    0x9d, 0xd2, 0xaf, 0x4a, 0xdf, 0x68, 0xee, 0x49, 0x8a, 0xa9, 0x34, 0x7d, 0x37, 0x56, 0x65, 0xa7,
];
const EFI_GLOBAL_VARIABLE_GUID: [u8; 16] = [
    0x61, 0xdf, 0xe4, 0x8b, 0xca, 0x93, 0xd2, 0x11, 0xaa, 0x0d, 0x00, 0xe0, 0x98, 0x03, 0x2b, 0x8c,
];
const EFI_IMAGE_SECURITY_DATABASE_GUID: [u8; 16] = [
    0xcb, 0xb2, 0x19, 0xd7, 0x3a, 0x3d, 0x96, 0x45, 0xa3, 0xbc, 0xda, 0xd0, 0x0e, 0x67, 0x65, 0x6f,
];

// NV | BS | RT | TIME_BASED_AUTHENTICATED_WRITE_ACCESS
const AUTH_VAR_ATTRS: u32 = 0x1 | 0x2 | 0x4 | 0x20;
const WIN_CERT_TYPE_EFI_GUID: u16 = 0x0ef1;
const EFIVARS_DIR: &str = "/sys/firmware/efi/efivars";

/// EFI_SIGNATURE_LIST holding a single X.509 certificate.
fn signature_list(cert_der: &[u8], owner_guid: &[u8; 16]) -> Vec<u8> {
    let signature_size = 16 + cert_der.len();
    let list_size = 28 + signature_size;
    let mut out = Vec::with_capacity(list_size);
    out.extend_from_slice(&EFI_CERT_X509_GUID);
    out.extend_from_slice(&(list_size as u32).to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // SignatureHeaderSize
    out.extend_from_slice(&(signature_size as u32).to_le_bytes());
    out.extend_from_slice(owner_guid);
    out.extend_from_slice(cert_der);
    out
}

/// A zero EFI_TIME; valid as the first timestamp when enrolling in Setup Mode.
fn efi_time_zero() -> [u8; 16] {
    [0u8; 16]
}

/// VariableName (CHAR16, no NUL) || VendorGuid || Attributes || TimeStamp || Data.
fn auth_message(name: &str, vendor_guid: &[u8; 16], data: &[u8]) -> Vec<u8> {
    let mut msg = Vec::new();
    for unit in name.encode_utf16() {
        msg.extend_from_slice(&unit.to_le_bytes());
    }
    msg.extend_from_slice(vendor_guid);
    msg.extend_from_slice(&AUTH_VAR_ATTRS.to_le_bytes());
    msg.extend_from_slice(&efi_time_zero());
    msg.extend_from_slice(data);
    msg
}

/// Detached PKCS#7 SignedData over `message` (pkcs7-data content type), produced
/// by OpenSSL — the canonical approach used by efitools for authenticated vars.
fn sign_detached_pkcs7(
    message: &[u8],
    signer_key: &PKeyRef<Private>,
    signer_cert: &X509,
) -> Result<Vec<u8>> {
    let certs = Stack::new()?;
    let pkcs7 = Pkcs7::sign(
        signer_cert,
        signer_key,
        &certs,
        message,
        Pkcs7Flags::DETACHED | Pkcs7Flags::BINARY | Pkcs7Flags::NOATTR,
    )?;
    Ok(pkcs7.to_der()?)
}

/// Build the full authenticated-variable payload (EFI_VARIABLE_AUTHENTICATION_2
/// descriptor followed by the EFI_SIGNATURE_LIST).
fn authenticated_payload(
    name: &str,
    vendor_guid: &[u8; 16],
    esl: &[u8],
    signer_key: &PKeyRef<Private>,
    signer_cert: &X509,
) -> Result<Vec<u8>> {
    let message = auth_message(name, vendor_guid, esl);
    let pkcs7 = sign_detached_pkcs7(&message, signer_key, signer_cert)?;

    let mut out = Vec::new();
    out.extend_from_slice(&efi_time_zero()); // EFI_TIME timestamp
    // WIN_CERTIFICATE_UEFI_GUID header.
    let win_cert_len = 8 + 16 + pkcs7.len();
    out.extend_from_slice(&(win_cert_len as u32).to_le_bytes()); // dwLength
    out.extend_from_slice(&WIN_CERT_REVISION_2_0.to_le_bytes());
    out.extend_from_slice(&WIN_CERT_TYPE_EFI_GUID.to_le_bytes());
    out.extend_from_slice(&EFI_CERT_TYPE_PKCS7_GUID);
    out.extend_from_slice(&pkcs7);
    out.extend_from_slice(esl);
    Ok(out)
}

/// Write an authenticated EFI variable via efivarfs.
fn write_auth_variable(name: &str, vendor_guid: &[u8; 16], payload: &[u8]) -> Result<()> {
    use std::io::Write;

    let guid_str = uuid::Uuid::from_bytes_le(*vendor_guid)
        .hyphenated()
        .to_string();
    let path = format!("{EFIVARS_DIR}/{name}-{guid_str}");

    // efivarfs expects one write of: attributes(u32 LE) || payload.
    let mut buf = Vec::with_capacity(4 + payload.len());
    buf.extend_from_slice(&AUTH_VAR_ATTRS.to_le_bytes());
    buf.extend_from_slice(payload);

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| anyhow!("failed to open {path}: {e}"))?;
    file.write_all(&buf)
        .map_err(|e| anyhow!("failed to write {path}: {e}"))?;
    file.sync_all()
        .map_err(|e| anyhow!("failed to sync {path}: {e}"))?;
    Ok(())
}

/// Enroll PK/KEK/db from a sbctl-style keydir. Enrolls db and KEK first, then
/// PK last (enrolling PK leaves Setup Mode). Replaces `sbctl enroll-keys`.
pub fn enroll_keys(keydir: &std::path::Path, owner_guid: &[u8; 16]) -> Result<()> {
    let read_pair = |kind: &str| -> Result<(PKey<Private>, X509)> {
        let dir = keydir.join(kind);
        let key_pem = std::fs::read_to_string(dir.join(kind).with_extension("key"))
            .map_err(|e| anyhow!("failed to read {kind} key: {e}"))?;
        let cert_pem = std::fs::read_to_string(dir.join(kind).with_extension("pem"))
            .map_err(|e| anyhow!("failed to read {kind} certificate: {e}"))?;
        load_key_and_cert(&key_pem, &cert_pem)
    };

    let (pk_key, pk_cert) = read_pair("PK")?;
    let (kek_key, kek_cert) = read_pair("KEK")?;
    let (_db_key, db_cert) = read_pair("db")?;

    let cert_der = |c: &X509| -> Result<Vec<u8>> { Ok(c.to_der()?) };

    // db: payload db cert, signed by KEK, stored under the image security db GUID.
    let esl = signature_list(&cert_der(&db_cert)?, owner_guid);
    let payload = authenticated_payload(
        "db",
        &EFI_IMAGE_SECURITY_DATABASE_GUID,
        &esl,
        &kek_key,
        &kek_cert,
    )?;
    write_auth_variable("db", &EFI_IMAGE_SECURITY_DATABASE_GUID, &payload)?;

    // KEK: payload KEK cert, signed by PK.
    let esl = signature_list(&cert_der(&kek_cert)?, owner_guid);
    let payload = authenticated_payload("KEK", &EFI_GLOBAL_VARIABLE_GUID, &esl, &pk_key, &pk_cert)?;
    write_auth_variable("KEK", &EFI_GLOBAL_VARIABLE_GUID, &payload)?;

    // PK: payload PK cert, self-signed; enrolling this leaves Setup Mode.
    let esl = signature_list(&cert_der(&pk_cert)?, owner_guid);
    let payload = authenticated_payload("PK", &EFI_GLOBAL_VARIABLE_GUID, &esl, &pk_key, &pk_cert)?;
    write_auth_variable("PK", &EFI_GLOBAL_VARIABLE_GUID, &payload)?;

    Ok(())
}

// --- On-target key material (replaces `sbctl create-keys` and key layout) ---

const SBCTL_KEY_PATHS: &[&str] = &[
    "var/lib/sbctl/keys/PK/PK.key",
    "var/lib/sbctl/keys/KEK/KEK.key",
    "var/lib/sbctl/keys/db/db.key",
];

fn target_sbctl_keys_exist() -> bool {
    let target = Path::new(TARGET_MOUNT);
    SBCTL_KEY_PATHS.iter().all(|p| target.join(p).is_file())
}

fn target_sbctl_dir() -> PathBuf {
    let path = Path::new(TARGET_MOUNT).join("var/lib/sbctl");
    let _ = std::fs::create_dir_all(&path);
    path
}

/// Directory holding the generated PK/KEK/db key pairs.
pub fn sbctl_keydir() -> PathBuf {
    target_sbctl_dir().join("keys")
}

/// Signature owner GUID (from the generated GUID file) in EFI byte order.
pub fn sbctl_owner_guid() -> [u8; 16] {
    let contents = std::fs::read_to_string(target_sbctl_dir().join("GUID")).unwrap_or_default();
    uuid::Uuid::parse_str(contents.trim())
        .unwrap_or(uuid::Uuid::nil())
        .to_bytes_le()
}

pub fn target_efi_binaries() -> Vec<PathBuf> {
    let esp = Path::new(TARGET_EFI_MOUNT);
    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in walkdir_like(esp, "*.efi") {
        if entry.is_file() {
            paths.push(entry);
        }
    }
    paths.sort();
    paths
}

fn walkdir_like(root: &Path, glob: &str) -> Vec<PathBuf> {
    let mut results = Vec::new();
    let ext_lower = glob.trim_start_matches("*.").to_lowercase();
    #[allow(clippy::items_after_statements)]
    fn recurse(dir: &Path, ext: &str, results: &mut Vec<PathBuf>) {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    recurse(&path, ext, results);
                } else if path
                    .extension()
                    .and_then(|e| e.to_str())
                    .is_some_and(|e| e.to_lowercase() == ext)
                {
                    results.push(path);
                }
            }
        }
    }
    recurse(root, &ext_lower, &mut results);
    results
}

fn random_uuid() -> Result<String> {
    let buf = crate::engine::util::random_bytes::<16>()?;
    Ok(uuid::Builder::from_random_bytes(buf)
        .into_uuid()
        .to_string())
}

/// Generate an RSA-2048 key and a self-signed X.509 certificate for a Secure
/// Boot key (PK/KEK/db), returning their PEM encodings. Replaces the keygen
/// side of `sbctl create-keys`.
pub(crate) fn generate_sb_key(common_name: &str) -> Result<(String, String)> {
    use openssl::asn1::Asn1Time;
    use openssl::bn::{BigNum, MsbOption};
    use openssl::rsa::Rsa;
    use openssl::x509::X509NameBuilder;

    let rsa = Rsa::generate(2048)?;
    let pkey = PKey::from_rsa(rsa)?;

    let mut name = X509NameBuilder::new()?;
    name.append_entry_by_text("CN", common_name)?;
    let name = name.build();

    let mut builder = X509::builder()?;
    builder.set_version(2)?; // X.509 v3
    let mut serial = BigNum::new()?;
    serial.rand(159, MsbOption::MAYBE_ZERO, false)?;
    let serial = serial.to_asn1_integer()?;
    builder.set_serial_number(&serial)?;
    builder.set_subject_name(&name)?;
    builder.set_issuer_name(&name)?;
    builder.set_pubkey(&pkey)?;
    let not_before = Asn1Time::days_from_now(0)?;
    let not_after = Asn1Time::days_from_now(3650)?;
    builder.set_not_before(&not_before)?;
    builder.set_not_after(&not_after)?;
    builder.sign(&pkey, MessageDigest::sha256())?;
    let cert = builder.build();

    let key_pem = String::from_utf8(pkey.private_key_to_pem_pkcs8()?)?;
    let cert_pem = String::from_utf8(cert.to_pem()?)?;
    Ok((key_pem, cert_pem))
}

pub fn ensure_sbctl_keys() -> Result<()> {
    if target_sbctl_keys_exist() {
        tracing::info!(
            target: crate::engine::INSTALLATION_LOG_TARGET,
            "Secure Boot signing keys already exist; reusing them"
        );
        return Ok(());
    }
    let keydir = target_sbctl_dir().join("keys");
    for kind in ["PK", "KEK", "db"] {
        let dir = keydir.join(kind);
        std::fs::create_dir_all(&dir)?;
        let (key_pem, cert_pem) = generate_sb_key(kind)?;
        let key_path = dir.join(kind).with_extension("key");
        std::fs::write(&key_path, key_pem)?;
        std::fs::set_permissions(&key_path, PermissionsExt::from_mode(0o600))?;
        std::fs::write(dir.join(kind).with_extension("pem"), cert_pem)?;
    }
    let guid_path = target_sbctl_dir().join("GUID");
    if !guid_path.exists() {
        std::fs::write(&guid_path, format!("{}\n", random_uuid()?))?;
    }
    tracing::info!(
        target: crate::engine::INSTALLATION_LOG_TARGET,
        "generated Secure Boot PK/KEK/db keys"
    );
    Ok(())
}
