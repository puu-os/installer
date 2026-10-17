// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#include "engine/der.h"

#include <array>
#include <cstddef>
#include <cstdint>
#include <limits>
#include <optional>
#include <span>
#include <string_view>
#include <vector>

#include "base/status.h"

namespace puu::engine::der {
namespace {

constexpr uint8_t kTagSequence = 0x30;
constexpr uint8_t kTagSet = 0x31;
constexpr uint8_t kTagOid = 0x06;

constexpr uint8_t kHighBit = 0x80;

constexpr uint64_t kMaxArc = std::numeric_limits<uint32_t>::max();

void AppendBase128(uint64_t arc, std::vector<uint8_t>& out) {
  std::array<uint8_t, 10> group = {};
  size_t count = 0;
  do {
    group[count++] = static_cast<uint8_t>(arc & 0x7f);
    arc >>= 7;
  } while (arc > 0);
  while (count > 1) {
    --count;
    out.push_back(static_cast<uint8_t>(group[count] | kHighBit));
  }
  out.push_back(group[0]);
}

StatusOr<uint64_t> ParseArc(std::string_view text, std::string_view dotted) {
  if (text.empty()) {
    return InvalidArgumentError("OID \"{}\" has an empty arc", dotted);
  }
  uint64_t value = 0;
  for (const char ch : text) {
    if (ch < '0' || ch > '9') {
      return InvalidArgumentError(R"(OID "{}" has a non-numeric arc "{}")",
                                  dotted, text);
    }
    value = value * 10 + static_cast<uint64_t>(ch - '0');
    if (value > kMaxArc) {
      return InvalidArgumentError(R"(OID "{}" has an out-of-range arc "{}")",
                                  dotted, text);
    }
  }
  return value;
}

StatusOr<std::vector<uint64_t>> ParseArcs(std::string_view dotted) {
  std::vector<uint64_t> arcs;
  std::string_view rest = dotted;
  while (true) {
    const size_t dot = rest.find('.');
    PUU_ASSIGN_OR_RETURN(const uint64_t arc,
                         ParseArc(rest.substr(0, dot), dotted));
    arcs.push_back(arc);
    if (dot == std::string_view::npos) {
      break;
    }
    rest.remove_prefix(dot + 1);
  }
  if (arcs.size() < 2) {
    return InvalidArgumentError("OID \"{}\" needs at least two arcs", dotted);
  }
  if (arcs[0] > 2) {
    return InvalidArgumentError("OID \"{}\" has a first arc above 2", dotted);
  }

  if (arcs[0] < 2 && arcs[1] >= 40) {
    return InvalidArgumentError(
        "OID \"{}\" has a second arc above 39 under arc {}", dotted, arcs[0]);
  }
  return arcs;
}

}  // namespace

std::vector<uint8_t> Length(size_t length) {
  if (length < kHighBit) {
    return {static_cast<uint8_t>(length)};
  }

  std::array<uint8_t, sizeof(size_t)> bytes = {};
  size_t count = 0;
  size_t rest = length;
  while (rest > 0) {
    bytes[count++] = static_cast<uint8_t>(rest & 0xff);
    rest >>= 8;
  }

  std::vector<uint8_t> out;
  out.reserve(1 + count);
  out.push_back(static_cast<uint8_t>(kHighBit | count));
  while (count > 0) {
    --count;
    out.push_back(bytes[count]);
  }
  return out;
}

std::vector<uint8_t> Tlv(uint8_t tag, std::span<const uint8_t> contents) {
  const std::vector<uint8_t> length = Length(contents.size());
  std::vector<uint8_t> out;
  out.reserve(1 + length.size() + contents.size());
  out.push_back(tag);
  out.insert(out.end(), length.begin(), length.end());
  out.insert(out.end(), contents.begin(), contents.end());
  return out;
}

std::vector<uint8_t> Sequence(std::span<const uint8_t> contents) {
  return Tlv(kTagSequence, contents);
}

std::vector<uint8_t> Set(std::span<const uint8_t> contents) {
  return Tlv(kTagSet, contents);
}

StatusOr<std::vector<uint8_t>> Oid(std::string_view dotted) {
  PUU_ASSIGN_OR_RETURN(const std::vector<uint64_t> arcs, ParseArcs(dotted));

  std::vector<uint8_t> body;
  AppendBase128((arcs[0] * 40) + arcs[1], body);
  for (size_t index = 2; index < arcs.size(); ++index) {
    AppendBase128(arcs[index], body);
  }
  return Tlv(kTagOid, body);
}

StatusOr<Element> ReadAt(std::span<const uint8_t> data, size_t offset) {
  if (offset > data.size()) {
    return InvalidArgumentError("DER offset past end of buffer");
  }
  const std::span<const uint8_t> input = data.subspan(offset);
  if (input.size() < 2) {
    return InvalidArgumentError("truncated DER element");
  }
  const uint8_t tag = input[0];

  if ((tag & 0x1f) == 0x1f) {
    return InvalidArgumentError("multi-byte DER tags are not supported");
  }
  const uint8_t first = input[1];

  size_t header = 2;
  size_t length = 0;
  if (first < kHighBit) {
    length = first;
  } else {
    const size_t count = first & 0x7f;
    if (count == 0) {
      return InvalidArgumentError("indefinite DER length is not allowed");
    }
    if (count == 0x7f) {
      return InvalidArgumentError("reserved DER length form");
    }
    if (count > sizeof(size_t)) {
      return InvalidArgumentError("DER length of {} octets is too large",
                                  count);
    }
    if (input.size() < 2 + count) {
      return InvalidArgumentError("truncated DER length");
    }
    if (input[2] == 0) {
      return InvalidArgumentError("non-minimal DER length");
    }
    for (size_t index = 0; index < count; ++index) {
      length = (length << 8) | input[2 + index];
    }
    if (length < kHighBit) {
      return InvalidArgumentError("non-minimal DER length");
    }
    header = 2 + count;
  }

  if (length > input.size() - header) {
    return InvalidArgumentError(
        "DER element of {} octets exceeds the {}-octet buffer", length,
        input.size());
  }
  const size_t total_size = header + length;
  return Element{
      .tag = tag,
      .contents = input.subspan(header, length),
      .total = data.subspan(offset, total_size),
      .next = offset + total_size,
  };
}

namespace {

std::optional<Element> TryReadAt(std::span<const uint8_t> data, size_t offset) {
  StatusOr<Element> parsed = ReadAt(data, offset);
  if (!parsed.ok()) {
    return std::nullopt;
  }
  return *parsed;
}

std::vector<uint8_t> Bytes(std::span<const uint8_t> data) {
  return {data.begin(), data.end()};
}

}  // namespace

std::optional<EmbeddedSignature> ParseAuthenticode(
    std::span<const uint8_t> pkcs7) {
  const std::optional<Element> content_info = TryReadAt(pkcs7, 0);
  if (!content_info.has_value() || content_info->tag != 0x30) {
    return std::nullopt;
  }
  const std::optional<Element> content_type =
      TryReadAt(content_info->contents, 0);
  if (!content_type.has_value()) {
    return std::nullopt;
  }
  const std::optional<Element> content =
      TryReadAt(content_info->contents, content_type->next);
  if (!content.has_value() || content->tag != 0xa0) {
    return std::nullopt;
  }
  const std::optional<Element> signed_data = TryReadAt(content->contents, 0);
  if (!signed_data.has_value() || signed_data->tag != 0x30) {
    return std::nullopt;
  }
  const std::span<const uint8_t> body = signed_data->contents;

  const std::optional<Element> version = TryReadAt(body, 0);
  if (!version.has_value()) {
    return std::nullopt;
  }
  const std::optional<Element> digest_algorithms =
      TryReadAt(body, version->next);
  if (!digest_algorithms.has_value()) {
    return std::nullopt;
  }
  const std::optional<Element> encap = TryReadAt(body, digest_algorithms->next);
  if (!encap.has_value()) {
    return std::nullopt;
  }

  const std::optional<Element> econtent_type = TryReadAt(encap->contents, 0);
  if (!econtent_type.has_value()) {
    return std::nullopt;
  }
  const std::optional<Element> econtent =
      TryReadAt(encap->contents, econtent_type->next);
  if (!econtent.has_value() || econtent->tag != 0xa0) {
    return std::nullopt;
  }

  const std::optional<Element> spc = TryReadAt(econtent->contents, 0);
  if (!spc.has_value()) {
    return std::nullopt;
  }
  const std::optional<Element> spc_data = TryReadAt(spc->contents, 0);
  if (!spc_data.has_value()) {
    return std::nullopt;
  }
  const std::optional<Element> digest_info =
      TryReadAt(spc->contents, spc_data->next);
  if (!digest_info.has_value()) {
    return std::nullopt;
  }
  const std::optional<Element> algorithm = TryReadAt(digest_info->contents, 0);
  if (!algorithm.has_value()) {
    return std::nullopt;
  }
  const std::optional<Element> digest =
      TryReadAt(digest_info->contents, algorithm->next);
  if (!digest.has_value() || digest->tag != 0x04) {
    return std::nullopt;
  }

  EmbeddedSignature out;
  out.digest = Bytes(digest->contents);
  size_t position = encap->next;
  while (position < body.size()) {
    const std::optional<Element> element = TryReadAt(body, position);
    if (!element.has_value()) {
      return std::nullopt;
    }
    position = element->next;
    if (element->tag != 0xa0) {
      continue;
    }

    size_t inner = 0;
    while (inner < element->contents.size()) {
      const std::optional<Element> certificate =
          TryReadAt(element->contents, inner);
      if (!certificate.has_value()) {
        return std::nullopt;
      }
      out.certificates.push_back(Bytes(certificate->total));
      inner = certificate->next;
    }
  }
  return out;
}

}  // namespace puu::engine::der
