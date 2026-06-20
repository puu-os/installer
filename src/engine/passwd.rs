// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

//! SHA-512 `crypt(3)` password hashing, replacing `openssl passwd -6 -stdin`.
//!
//! Implements Ulrich Drepper's `$6$` SHA-512 crypt scheme on top of OpenSSL's
//! SHA-512 primitive so that the only cryptographic backend is libcrypto.

use anyhow::Result;

/// crypt(3) base64 alphabet (`./0-9A-Za-z`), used for both the salt and the
/// encoded digest of a `$6$` hash.
const CRYPT_B64: &[u8; 64] = b"./0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// Produce a `$6$` SHA-512 crypt hash, replacing `openssl passwd -6 -stdin`.
pub fn hash_password_sha512(password: &str) -> Result<String> {
    // A random 16-character salt drawn from the crypt alphabet, as emitted by
    // `openssl passwd -6`. The alphabet is ASCII, so each byte is its own char.
    let salt_bytes = crate::engine::util::random_bytes::<16>()?;
    let salt: String = salt_bytes
        .iter()
        .map(|b| CRYPT_B64[(b & 0x3f) as usize] as char)
        .collect();
    let hash = sha512_crypt(password.as_bytes(), salt.as_bytes());
    Ok(format!("$6${salt}${hash}"))
}

/// SHA-512 crypt with the default 5000 rounds, returning the encoded digest
/// (the trailing `$6$<salt>$` field).
fn sha512_crypt(key: &[u8], salt: &[u8]) -> String {
    use openssl::sha::Sha512;

    const ROUNDS: usize = 5000;

    // Digest B = SHA512(key || salt || key).
    let mut temp_ctx = Sha512::new();
    temp_ctx.update(key);
    temp_ctx.update(salt);
    temp_ctx.update(key);
    let temp_result = temp_ctx.finish();

    // Digest A = SHA512(key || salt || B-extended-to-keylen || pattern-of(keylen)).
    let mut alt_ctx = Sha512::new();
    alt_ctx.update(key);
    alt_ctx.update(salt);
    let mut count = key.len();
    while count > 64 {
        alt_ctx.update(&temp_result);
        count -= 64;
    }
    alt_ctx.update(&temp_result[..count]);
    let mut bits = key.len();
    while bits > 0 {
        if bits & 1 != 0 {
            alt_ctx.update(&temp_result);
        } else {
            alt_ctx.update(key);
        }
        bits >>= 1;
    }
    let mut alt_result = alt_ctx.finish();

    // Sequence P: SHA512(key repeated keylen times), tiled to keylen bytes.
    let mut p_ctx = Sha512::new();
    for _ in 0..key.len() {
        p_ctx.update(key);
    }
    let key_digest = p_ctx.finish();
    let p_bytes = tile(&key_digest, key.len());

    // Sequence S: SHA512(salt repeated 16 + A[0] times), tiled to saltlen bytes.
    let mut s_ctx = Sha512::new();
    for _ in 0..(16 + alt_result[0] as usize) {
        s_ctx.update(salt);
    }
    let salt_digest = s_ctx.finish();
    let s_bytes = tile(&salt_digest, salt.len());

    // Iterate ROUNDS times, alternating the running digest with P and S.
    for round in 0..ROUNDS {
        let mut ctx = Sha512::new();
        if round & 1 != 0 {
            ctx.update(&p_bytes);
        } else {
            ctx.update(&alt_result);
        }
        if round % 3 != 0 {
            ctx.update(&s_bytes);
        }
        if round % 7 != 0 {
            ctx.update(&p_bytes);
        }
        if round & 1 != 0 {
            ctx.update(&alt_result);
        } else {
            ctx.update(&p_bytes);
        }
        alt_result = ctx.finish();
    }

    sha512_crypt_b64(&alt_result)
}

/// Repeat the 64-byte `digest` to fill `len` bytes.
fn tile(digest: &[u8; 64], len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut cnt = len;
    while cnt > 64 {
        out.extend_from_slice(digest);
        cnt -= 64;
    }
    out.extend_from_slice(&digest[..cnt]);
    out
}

/// crypt(3) base64 encoding of a 64-byte SHA-512 digest (86 characters), using
/// the byte permutation defined by the SHA-512 crypt scheme.
fn sha512_crypt_b64(digest: &[u8; 64]) -> String {
    // The crypt alphabet is ASCII, so each emitted byte is its own char.
    let mut out = String::with_capacity(86);
    let mut emit = |hi: u8, mid: u8, lo: u8, count: usize| {
        let mut word = (u32::from(hi) << 16) | (u32::from(mid) << 8) | u32::from(lo);
        for _ in 0..count {
            out.push(CRYPT_B64[(word & 0x3f) as usize] as char);
            word >>= 6;
        }
    };
    emit(digest[0], digest[21], digest[42], 4);
    emit(digest[22], digest[43], digest[1], 4);
    emit(digest[44], digest[2], digest[23], 4);
    emit(digest[3], digest[24], digest[45], 4);
    emit(digest[25], digest[46], digest[4], 4);
    emit(digest[47], digest[5], digest[26], 4);
    emit(digest[6], digest[27], digest[48], 4);
    emit(digest[28], digest[49], digest[7], 4);
    emit(digest[50], digest[8], digest[29], 4);
    emit(digest[9], digest[30], digest[51], 4);
    emit(digest[31], digest[52], digest[10], 4);
    emit(digest[53], digest[11], digest[32], 4);
    emit(digest[12], digest[33], digest[54], 4);
    emit(digest[34], digest[55], digest[13], 4);
    emit(digest[56], digest[14], digest[35], 4);
    emit(digest[15], digest[36], digest[57], 4);
    emit(digest[37], digest[58], digest[16], 4);
    emit(digest[59], digest[17], digest[38], 4);
    emit(digest[18], digest[39], digest[60], 4);
    emit(digest[40], digest[61], digest[19], 4);
    emit(digest[62], digest[20], digest[41], 4);
    emit(0, 0, digest[63], 2);
    out
}
