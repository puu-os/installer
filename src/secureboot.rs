// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

//! Native Secure Boot signing and key enrollment, replacing the `sbctl`
//! `sign`/`verify`/`enroll-keys` subcommands.
//!
//! All cryptography goes through OpenSSL (libcrypto) so the binary depends on a
//! single, FIPS-certifiable crypto backend. The Authenticode/PKCS#7 and PE
//! container structures are assembled and parsed with the small hand-rolled DER
//! helpers below rather than a third-party ASN.1 stack.

#![allow(clippy::doc_markdown)]

use anyhow::{Result, anyhow, bail};
use openssl::hash::MessageDigest;
use openssl::pkcs7::{Pkcs7, Pkcs7Flags};
use openssl::pkey::{PKey, PKeyRef, Private};
use openssl::sha::{Sha256, sha256};
use openssl::sign::Signer;
use openssl::stack::Stack;
use openssl::x509::X509;

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

// --- Minimal DER encoding ---

/// Encode a DER definite-length prefix.
fn der_len(len: usize) -> Vec<u8> {
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
fn der_tlv(tag: u8, contents: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(2 + contents.len());
    out.push(tag);
    out.extend_from_slice(&der_len(contents.len()));
    out.extend_from_slice(contents);
    out
}

/// SEQUENCE built from already-encoded elements.
fn der_seq(elems: &[Vec<u8>]) -> Vec<u8> {
    der_tlv(0x30, &elems.concat())
}

/// SET built from already-encoded elements. The caller orders the elements per
/// the DER SET OF rules (or passes a single element).
fn der_set(elems: &[Vec<u8>]) -> Vec<u8> {
    der_tlv(0x31, &elems.concat())
}

fn der_octet(bytes: &[u8]) -> Vec<u8> {
    der_tlv(0x04, bytes)
}

fn der_int_u8(value: u8) -> Vec<u8> {
    der_tlv(0x02, &[value])
}

fn der_null() -> Vec<u8> {
    vec![0x05, 0x00]
}

/// Encode an OBJECT IDENTIFIER from its arc values.
fn der_oid(arcs: &[u32]) -> Vec<u8> {
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
fn algorithm_identifier(oid: &[u32]) -> Vec<u8> {
    der_seq(&[der_oid(oid), der_null()])
}

// --- Minimal DER decoding ---

/// A single DER element: its tag, its contents, and the full `tag||len||value`
/// slice it occupies.
struct Tlv<'a> {
    tag: u8,
    contents: &'a [u8],
    total: &'a [u8],
}

/// Read the TLV at `data[pos..]`, returning it and the offset just past it.
fn der_read(data: &[u8], pos: usize) -> Result<(Tlv<'_>, usize)> {
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

    // Signed attributes: content-type = SPC_INDIRECT_DATA, message-digest = SHA-256(spc).
    let content_digest = sha256(&spc);
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
    let sec_dir = security_dir_offset(image)?;
    let table_off = read_u32_le(image, sec_dir)? as usize;
    let table_size = read_u32_le(image, sec_dir + 4)? as usize;
    if table_size == 0 {
        return Ok(Vec::new());
    }
    let table_end = table_off + table_size;
    let mut out = Vec::new();
    let mut pos = table_off;
    while pos + 8 <= table_end {
        let cert_size = read_u32_le(image, pos)? as usize;
        let revision = read_u16_le(image, pos + 4)?;
        let cert_type = read_u16_le(image, pos + 6)?;
        if cert_size < 8 {
            break;
        }
        let data = image
            .get(pos + 8..pos + cert_size)
            .ok_or_else(|| anyhow!("certificate entry exceeds table"))?;
        if revision == WIN_CERT_REVISION_2_0 && cert_type == WIN_CERT_TYPE_PKCS_SIGNED_DATA {
            out.push(data);
        }
        pos += (cert_size + 7) & !7; // 8-byte aligned entries
    }
    Ok(out)
}

/// Authenticode-sign the PE image bytes, returning the signed image.
pub fn sign_pe(image: &[u8], key_pem: &str, cert_pem: &str) -> Result<Vec<u8>> {
    let (key, cert) = load_key_and_cert(key_pem, cert_pem)?;

    let digest = authenticode_sha256(image)?;
    let pkcs7 = build_pkcs7(&digest, &key, &cert)?;

    // WIN_CERTIFICATE: dwLength (incl. 8-byte header) | wRevision | wType | data,
    // padded so the whole entry is 8-byte aligned.
    let cert_len = 8 + pkcs7.len();
    let mut entry = Vec::with_capacity((cert_len + 7) & !7);
    entry.extend_from_slice(&(cert_len as u32).to_le_bytes());
    entry.extend_from_slice(&WIN_CERT_REVISION_2_0.to_le_bytes());
    entry.extend_from_slice(&WIN_CERT_TYPE_PKCS_SIGNED_DATA.to_le_bytes());
    entry.extend_from_slice(&pkcs7);
    while entry.len() % 8 != 0 {
        entry.push(0);
    }

    // The certificate table is appended at the (8-aligned) end of the image.
    let mut out = image.to_vec();
    while out.len() % 8 != 0 {
        out.push(0);
    }
    let cert_offset = out.len();
    out.extend_from_slice(&entry);

    // Point the security data directory at the appended certificate table.
    let dir = security_dir_offset(image)?;
    out[dir..dir + 4].copy_from_slice(&(cert_offset as u32).to_le_bytes());
    out[dir + 4..dir + 8].copy_from_slice(&(entry.len() as u32).to_le_bytes());

    Ok(out)
}

/// Verify that the PE image carries an Authenticode signature whose digest
/// matches the image and whose signer matches `cert_pem`.
pub fn verify_pe(image: &[u8], cert_pem: &str) -> Result<bool> {
    let Ok(cert) = X509::from_pem(cert_pem.as_bytes()) else {
        return Ok(false);
    };
    let cert_der = cert.to_der()?;
    let expected = authenticode_sha256(image)?;

    for pkcs7 in attribute_certificates(image)? {
        let Some((digest, certs)) = parse_authenticode(pkcs7) else {
            continue;
        };
        if digest != expected {
            continue;
        }
        if certs.iter().any(|c| c == &cert_der) {
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

    let guid_str = format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        vendor_guid[3],
        vendor_guid[2],
        vendor_guid[1],
        vendor_guid[0],
        vendor_guid[5],
        vendor_guid[4],
        vendor_guid[7],
        vendor_guid[6],
        vendor_guid[8],
        vendor_guid[9],
        vendor_guid[10],
        vendor_guid[11],
        vendor_guid[12],
        vendor_guid[13],
        vendor_guid[14],
        vendor_guid[15],
    );
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
    Ok(())
}

/// Enroll PK/KEK/db from a sbctl-style keydir. Enrolls db and KEK first, then
/// PK last (enrolling PK leaves Setup Mode). Replaces `sbctl enroll-keys`.
pub fn enroll_keys(keydir: &std::path::Path, owner_guid: &[u8; 16]) -> Result<()> {
    let read_pair = |kind: &str| -> Result<(PKey<Private>, X509)> {
        let dir = keydir.join(kind);
        let key_pem = std::fs::read_to_string(dir.join(format!("{kind}.key")))
            .map_err(|e| anyhow!("failed to read {kind} key: {e}"))?;
        let cert_pem = std::fs::read_to_string(dir.join(format!("{kind}.pem")))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn der_length_encoding() {
        assert_eq!(der_len(0), vec![0x00]);
        assert_eq!(der_len(127), vec![0x7f]);
        assert_eq!(der_len(128), vec![0x81, 0x80]);
        assert_eq!(der_len(255), vec![0x81, 0xff]);
        assert_eq!(der_len(256), vec![0x82, 0x01, 0x00]);
        assert_eq!(der_len(0x1234), vec![0x82, 0x12, 0x34]);
    }

    #[test]
    fn der_oid_encoding() {
        // SHA-256 (2.16.840.1.101.3.4.2.1).
        assert_eq!(
            der_oid(OID_SHA256),
            vec![
                0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01
            ]
        );
        // rsaEncryption (1.2.840.113549.1.1.1).
        assert_eq!(
            der_oid(OID_RSA_ENCRYPTION),
            vec![
                0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01
            ]
        );
        // SPC_INDIRECT_DATA (1.3.6.1.4.1.311.2.1.4), exercising a multi-byte arc.
        assert_eq!(
            der_oid(OID_SPC_INDIRECT_DATA),
            vec![
                0x06, 0x0a, 0x2b, 0x06, 0x01, 0x04, 0x01, 0x82, 0x37, 0x02, 0x01, 0x04
            ]
        );
    }

    #[test]
    fn der_read_roundtrip() {
        let encoded = der_seq(&[der_int_u8(1), der_octet(&[0xaa, 0xbb])]);
        let (seq, end) = der_read(&encoded, 0).unwrap();
        assert_eq!(seq.tag, 0x30);
        assert_eq!(end, encoded.len());
        let (int, next) = der_read(seq.contents, 0).unwrap();
        assert_eq!(int.tag, 0x02);
        assert_eq!(int.contents, &[1]);
        let (octet, _) = der_read(seq.contents, next).unwrap();
        assert_eq!(octet.tag, 0x04);
        assert_eq!(octet.contents, &[0xaa, 0xbb]);
    }

    #[test]
    fn issuer_and_serial_extracts_from_self_signed_cert() {
        // A self-signed RSA certificate generated by OpenSSL, the same shape as
        // the Secure Boot keys this module produces.
        let rsa = openssl::rsa::Rsa::generate(2048).unwrap();
        let pkey = openssl::pkey::PKey::from_rsa(rsa).unwrap();
        let mut name = openssl::x509::X509NameBuilder::new().unwrap();
        name.append_entry_by_text("CN", "test").unwrap();
        let name = name.build();
        let mut builder = openssl::x509::X509::builder().unwrap();
        builder.set_version(2).unwrap();
        let serial = openssl::bn::BigNum::from_u32(0x0102_0304)
            .unwrap()
            .to_asn1_integer()
            .unwrap();
        builder.set_serial_number(&serial).unwrap();
        builder.set_subject_name(&name).unwrap();
        builder.set_issuer_name(&name).unwrap();
        builder.set_pubkey(&pkey).unwrap();
        let not_before = openssl::asn1::Asn1Time::days_from_now(0).unwrap();
        let not_after = openssl::asn1::Asn1Time::days_from_now(1).unwrap();
        builder.set_not_before(&not_before).unwrap();
        builder.set_not_after(&not_after).unwrap();
        builder
            .sign(&pkey, openssl::hash::MessageDigest::sha256())
            .unwrap();
        let cert = builder.build();
        let cert_der = cert.to_der().unwrap();

        let (issuer, serial) = issuer_and_serial(&cert_der).unwrap();
        // The serial number INTEGER carries the value we set.
        assert_eq!(serial, vec![0x02, 0x04, 0x01, 0x02, 0x03, 0x04]);
        // The issuer is a Name SEQUENCE that appears verbatim in the cert DER.
        assert_eq!(issuer[0], 0x30);
        assert!(
            cert_der
                .windows(issuer.len())
                .any(|window| window == issuer.as_slice())
        );
    }

    /// A minimal but structurally valid PE32+ image: a DOS header, a PE/COFF
    /// header with one section, a full optional header with 16 data directories
    /// (so the security directory at index 4 exists), and 512 bytes of section
    /// data. Just enough for the Authenticode digest and certificate table to be
    /// well defined.
    fn minimal_pe() -> Vec<u8> {
        fn put16(image: &mut [u8], off: usize, value: u16) {
            image[off..off + 2].copy_from_slice(&value.to_le_bytes());
        }
        fn put32(image: &mut [u8], off: usize, value: u32) {
            image[off..off + 4].copy_from_slice(&value.to_le_bytes());
        }

        let mut image = vec![0u8; 1024];
        put32(&mut image, 0x3c, 64); // e_lfanew
        image[64..68].copy_from_slice(b"PE\0\0");
        put16(&mut image, 70, 1); // NumberOfSections
        put16(&mut image, 84, 240); // SizeOfOptionalHeader
        let opt = 88;
        put16(&mut image, opt, 0x20b); // PE32+ magic
        put32(&mut image, opt + 60, 512); // SizeOfHeaders
        // Single section header at opt + SizeOfOptionalHeader.
        let section = opt + 240;
        put32(&mut image, section + 16, 512); // SizeOfRawData
        put32(&mut image, section + 20, 512); // PointerToRawData
        image
    }

    #[test]
    fn sign_and_verify_roundtrip() {
        let (key_pem, cert_pem) = crate::pipeline::generate_sb_key("roundtrip").unwrap();
        let image = minimal_pe();

        // An unsigned image carries no matching signature.
        assert!(!verify_pe(&image, &cert_pem).unwrap());

        let signed = sign_pe(&image, &key_pem, &cert_pem).unwrap();
        assert!(verify_pe(&signed, &cert_pem).unwrap());

        // The hand-rolled DER must also parse under OpenSSL's independent ASN.1
        // decoder, cross-checking the length encoding our own parser produced.
        let blobs = attribute_certificates(&signed).unwrap();
        assert_eq!(blobs.len(), 1);
        assert!(Pkcs7::from_der(blobs[0]).is_ok());

        // A signature made by one key must not verify against a different cert.
        let (_other_key, other_cert) = crate::pipeline::generate_sb_key("other").unwrap();
        assert!(!verify_pe(&signed, &other_cert).unwrap());

        // Tampering with hashed section data invalidates the signature.
        let mut tampered = signed.clone();
        tampered[512] ^= 0xff;
        assert!(!verify_pe(&tampered, &cert_pem).unwrap());
    }
}
