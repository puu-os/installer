// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

//! Native Secure Boot signing and key enrollment, replacing the `sbctl`
//! `sign`/`verify`/`enroll-keys` subcommands.
//!
//! NOTE: the produced Authenticode signatures and authenticated EFI variables
//! follow the PE/UEFI specifications but have not been validated against real
//! firmware in this environment; they require OVMF Secure Boot validation.
#![allow(clippy::doc_markdown)]

use anyhow::{Result, anyhow, bail};
use openssl::hash::MessageDigest;
use openssl::pkcs7::{Pkcs7, Pkcs7Flags};
use openssl::pkey::{PKey, PKeyRef, Private};
use openssl::sign::Signer;
use openssl::stack::Stack;
use openssl::x509::X509;
use sha2::{Digest, Sha256};

use cms::builder::{create_content_type_attribute, create_message_digest_attribute};
use cms::cert::{CertificateChoices, IssuerAndSerialNumber};
use cms::content_info::{CmsVersion, ContentInfo};
use cms::signed_data::{
    CertificateSet, EncapsulatedContentInfo, SignedData, SignerIdentifier, SignerInfo, SignerInfos,
};
use x509_cert::Certificate;
use x509_cert::attr::Attributes;
use x509_cert::der::asn1::{Null, OctetString, SetOfVec};
use x509_cert::der::oid::ObjectIdentifier;
use x509_cert::der::{Any, Decode, Encode};
use x509_cert::spki::AlgorithmIdentifierOwned;

use authenticode::{
    AttributeCertificateIterator, DigestInfo, SpcAttributeTypeAndOptionalValue,
    SpcIndirectDataContent, authenticode_digest,
};

const OID_SPC_INDIRECT_DATA: ObjectIdentifier =
    ObjectIdentifier::new_unwrap("1.3.6.1.4.1.311.2.1.4");
const OID_SPC_PE_IMAGE_DATA: ObjectIdentifier =
    ObjectIdentifier::new_unwrap("1.3.6.1.4.1.311.2.1.15");
const OID_SHA256: ObjectIdentifier = ObjectIdentifier::new_unwrap("2.16.840.1.101.3.4.2.1");
const OID_RSA_ENCRYPTION: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.1.1");
const OID_SIGNED_DATA: ObjectIdentifier = ObjectIdentifier::new_unwrap("1.2.840.113549.1.7.2");

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

fn sha256_algorithm_identifier() -> AlgorithmIdentifierOwned {
    AlgorithmIdentifierOwned {
        oid: OID_SHA256,
        parameters: Some(Any::from(Null)),
    }
}

fn rsa_encryption_identifier() -> AlgorithmIdentifierOwned {
    AlgorithmIdentifierOwned {
        oid: OID_RSA_ENCRYPTION,
        parameters: Some(Any::from(Null)),
    }
}

/// Parse a PEM private key and certificate pair via OpenSSL.
fn load_key_and_cert(key_pem: &str, cert_pem: &str) -> Result<(PKey<Private>, X509)> {
    let key = PKey::private_key_from_pem(key_pem.as_bytes())?;
    let cert = X509::from_pem(cert_pem.as_bytes())?;
    Ok((key, cert))
}

/// Build the Authenticode PKCS#7 SignedData blob for a precomputed PE digest.
/// The ASN.1 structure is assembled with the `cms`/`der` types while the RSA
/// signature over the signed attributes is produced by OpenSSL.
fn build_pkcs7(digest: &[u8], key: &PKeyRef<Private>, cert: &X509) -> Result<Vec<u8>> {
    let spc = SpcIndirectDataContent {
        data: SpcAttributeTypeAndOptionalValue {
            value_type: OID_SPC_PE_IMAGE_DATA,
            value: Any::from_der(SPC_PE_IMAGE_DATA_VALUE)
                .map_err(|e| anyhow!("invalid SpcPeImageData: {e}"))?,
        },
        message_digest: DigestInfo {
            digest_algorithm: sha256_algorithm_identifier(),
            digest: OctetString::new(digest.to_vec())
                .map_err(|e| anyhow!("invalid digest octet string: {e}"))?,
        },
    };
    let spc_der = spc
        .to_der()
        .map_err(|e| anyhow!("failed to encode SPC content: {e}"))?;

    // Signed attributes: content-type = SPC_INDIRECT_DATA, message-digest = SHA-256(spc_der).
    let content_digest = Sha256::digest(&spc_der);
    let mut attrs: Attributes = SetOfVec::new();
    attrs
        .insert(
            create_content_type_attribute(OID_SPC_INDIRECT_DATA)
                .map_err(|e| anyhow!("content-type attribute: {e}"))?,
        )
        .map_err(|e| anyhow!("attribute set: {e}"))?;
    attrs
        .insert(
            create_message_digest_attribute(&content_digest)
                .map_err(|e| anyhow!("message-digest attribute: {e}"))?,
        )
        .map_err(|e| anyhow!("attribute set: {e}"))?;
    let signed_attrs_der = attrs
        .to_der()
        .map_err(|e| anyhow!("encode signed attrs: {e}"))?;

    // RSA-SHA256 signature over the DER SET OF signed attributes (OpenSSL).
    let mut signer = Signer::new(MessageDigest::sha256(), key)?;
    signer.update(&signed_attrs_der)?;
    let signature = signer.sign_to_vec()?;

    let xc = Certificate::from_der(&cert.to_der()?)
        .map_err(|e| anyhow!("failed to parse certificate DER: {e}"))?;
    let sid = SignerIdentifier::IssuerAndSerialNumber(IssuerAndSerialNumber {
        issuer: xc.tbs_certificate.issuer.clone(),
        serial_number: xc.tbs_certificate.serial_number.clone(),
    });

    let signer_info = SignerInfo {
        version: CmsVersion::V1,
        sid,
        digest_alg: sha256_algorithm_identifier(),
        signed_attrs: Some(attrs),
        signature_algorithm: rsa_encryption_identifier(),
        signature: OctetString::new(signature).map_err(|e| anyhow!("signature octets: {e}"))?,
        unsigned_attrs: None,
    };

    let mut digest_algorithms: SetOfVec<AlgorithmIdentifierOwned> = SetOfVec::new();
    digest_algorithms
        .insert(sha256_algorithm_identifier())
        .map_err(|e| anyhow!("digest-algorithm set: {e}"))?;
    let mut signer_infos: SetOfVec<SignerInfo> = SetOfVec::new();
    signer_infos
        .insert(signer_info)
        .map_err(|e| anyhow!("signer-info set: {e}"))?;
    let mut certificates: SetOfVec<CertificateChoices> = SetOfVec::new();
    certificates
        .insert(CertificateChoices::Certificate(xc))
        .map_err(|e| anyhow!("certificate set: {e}"))?;

    let signed_data = SignedData {
        version: CmsVersion::V1,
        digest_algorithms,
        encap_content_info: EncapsulatedContentInfo {
            econtent_type: OID_SPC_INDIRECT_DATA,
            econtent: Some(Any::from_der(&spc_der).map_err(|e| anyhow!("wrap SPC: {e}"))?),
        },
        certificates: Some(CertificateSet(certificates)),
        crls: None,
        signer_infos: SignerInfos(signer_infos),
    };

    let content_info = ContentInfo {
        content_type: OID_SIGNED_DATA,
        content: Any::from_der(
            &signed_data
                .to_der()
                .map_err(|e| anyhow!("encode SignedData: {e}"))?,
        )
        .map_err(|e| anyhow!("wrap SignedData: {e}"))?,
    };
    content_info
        .to_der()
        .map_err(|e| anyhow!("encode ContentInfo: {e}"))
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

/// Offset of the IMAGE_DATA_DIRECTORY for the certificate table (index 4)
/// within the PE optional header.
fn security_dir_offset(data: &[u8]) -> Result<usize> {
    let e_lfanew = read_u32_le(data, 0x3c)? as usize;
    // PE signature (4) + COFF file header (20) -> optional header.
    let opt = e_lfanew + 24;
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

/// Authenticode-sign the PE image bytes, returning the signed image.
pub fn sign_pe(image: &[u8], key_pem: &str, cert_pem: &str) -> Result<Vec<u8>> {
    use object::read::pe::PeFile64;

    let (key, cert) = load_key_and_cert(key_pem, cert_pem)?;

    let pe = PeFile64::parse(image).map_err(|e| anyhow!("failed to parse PE image: {e}"))?;
    let mut hasher = Sha256::new();
    authenticode_digest(&pe, &mut hasher)
        .map_err(|e| anyhow!("authenticode digest failed: {e}"))?;
    let digest = hasher.finalize();

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
    use object::read::pe::PeFile64;

    let Ok(cert) = X509::from_pem(cert_pem.as_bytes()) else {
        return Ok(false);
    };
    let cert_der = cert.to_der()?;

    let pe = PeFile64::parse(image).map_err(|e| anyhow!("failed to parse PE image: {e}"))?;
    let mut hasher = Sha256::new();
    authenticode_digest(&pe, &mut hasher)
        .map_err(|e| anyhow!("authenticode digest failed: {e}"))?;
    let expected = hasher.finalize();

    let Some(mut iter) = AttributeCertificateIterator::new(&pe)
        .map_err(|e| anyhow!("failed to read certificate table: {e}"))?
    else {
        return Ok(false);
    };
    while let Some(attr) = iter
        .next()
        .transpose()
        .map_err(|e| anyhow!("failed to iterate certificates: {e}"))?
    {
        let Ok(sig) = attr.get_authenticode_signature() else {
            continue;
        };
        if sig.digest() != expected.as_slice() {
            continue;
        }
        let signer_matches = sig
            .certificates()
            .any(|c| c.to_der().is_ok_and(|d| d == cert_der));
        if signer_matches {
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

