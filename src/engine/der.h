// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026
//
// The minimum DER needed to assemble an Authenticode signature and an EFI
// signature list. OpenSSL cannot emit these structures directly, and a full
// ASN.1 library would be far more surface than the four shapes used here.

#ifndef PUU_ENGINE_DER_H_
#define PUU_ENGINE_DER_H_

#include <cstdint>
#include <optional>
#include <span>
#include <string_view>
#include <vector>

#include "base/status.h"

namespace puu::engine::der {

// Encodes a definite-form length.
std::vector<uint8_t> Length(size_t length);

// Encodes one tag-length-value element.
std::vector<uint8_t> Tlv(uint8_t tag, std::span<const uint8_t> contents);

// Encodes a SEQUENCE holding the already-encoded `contents`.
std::vector<uint8_t> Sequence(std::span<const uint8_t> contents);

// Encodes a SET holding the already-encoded `contents`.
std::vector<uint8_t> Set(std::span<const uint8_t> contents);

// Encodes an OBJECT IDENTIFIER from dotted-decimal text, for example
// "1.2.840.113549.1.7.2".
StatusOr<std::vector<uint8_t>> Oid(std::string_view dotted);

// One parsed element.
struct Element {
  uint8_t tag = 0;
  // Borrowed from the buffer passed to Read; valid only while it lives.
  std::span<const uint8_t> contents;
  // Total encoded size, for advancing to the next element.
  size_t total_size = 0;
};

// Parses the element at the start of `input`.
StatusOr<Element> Read(std::span<const uint8_t> input);

// One parsed element together with the offset just past it, mirroring the
// (element, next) pair sequential parsing needs.
struct Parsed {
  uint8_t tag = 0;
  std::span<const uint8_t> contents;
  std::span<const uint8_t> total;
  size_t next = 0;
};

// Parses the element at `offset` within `data`.
StatusOr<Parsed> ReadAt(std::span<const uint8_t> data, size_t offset);

// As ReadAt, but nullopt instead of a Status on malformed input; for callers
// that skip malformed elements rather than report them.
std::optional<Parsed> TryReadAt(std::span<const uint8_t> data, size_t offset);

// The Authenticode digest plus signer certificates extracted from a PKCS#7
// blob.
struct EmbeddedSignature {
  std::vector<uint8_t> digest;
  std::vector<std::vector<uint8_t>> certificates;
};

// Pulls the Authenticode digest and the signer certificates out of a PKCS#7
// SignedData. Returns nullopt when the structure is not Authenticode-shaped;
// a malformed blob is skipped rather than trusted.
std::optional<EmbeddedSignature> ParseAuthenticode(
    std::span<const uint8_t> pkcs7);

}  // namespace puu::engine::der

#endif  // PUU_ENGINE_DER_H_
