// SPDX-License-Identifier: GPL-2.0-or-later
// Copyright (C) Opinsys Oy 2026

#include "engine/secureboot.h"

#include <algorithm>
#include <array>
#include <cctype>
#include <cerrno>
#include <cstddef>
#include <cstdint>
#include <ctime>
#include <filesystem>
#include <initializer_list>
#include <memory>
#include <optional>
#include <span>
#include <string>
#include <string_view>
#include <system_error>
#include <utility>
#include <vector>

#include <absl/status/status.h>
#include <fmt/format.h>
#include <openssl/asn1.h>
#include <openssl/bio.h>
#include <openssl/bn.h>
#include <openssl/crypto.h>
#include <openssl/err.h>
#include <openssl/evp.h>
#include <openssl/pem.h>
#include <openssl/pkcs7.h>
#include <openssl/rsa.h>
#include <openssl/safestack.h>
#include <openssl/x509.h>
#include <openssl/x509_vfy.h>

#include "base/status.h"
#include "engine/der.h"
#include "engine/firmware.h"
#include "engine/le_bytes.h"
#include "engine/util.h"

namespace puu::engine {
namespace {

std::vector<uint8_t> BuildSignatureList(std::span<const uint8_t> cert_der,
                                        const EfiGuid& owner);

size_t AlignCertificateEntry(size_t size);

struct AuthenticodeLayout {
  size_t optional_header_offset = 0;
  // IMAGE_OPTIONAL_HEADER.CheckSum, 4 bytes, skipped: signing changes it.
  size_t checksum_offset = 0;

  size_t security_dir_offset = 0;

  size_t size_of_headers = 0;
  size_t number_of_sections = 0;
  size_t section_table_offset = 0;
  size_t certificate_table_offset = 0;
  size_t certificate_table_size = 0;
};

constexpr std::string_view kOidSpcIndirectData = "1.3.6.1.4.1.311.2.1.4";
constexpr std::string_view kOidSpcPeImageData = "1.3.6.1.4.1.311.2.1.15";
constexpr std::string_view kOidSha256 = "2.16.840.1.101.3.4.2.1";
constexpr std::string_view kOidRsaEncryption = "1.2.840.113549.1.1.1";
constexpr std::string_view kOidSignedData = "1.2.840.113549.1.7.2";
constexpr std::string_view kOidContentType = "1.2.840.113549.1.9.3";
constexpr std::string_view kOidMessageDigest = "1.2.840.113549.1.9.4";

constexpr uint16_t kWinCertRevision20 = 0x0200;
constexpr uint16_t kWinCertTypePkcsSignedData = 0x0002;
constexpr uint16_t kWinCertTypeEfiGuid = 0x0ef1;

constexpr uint32_t kAuthVariableAttributes = 0x1 | 0x2 | 0x4 | 0x20;

constexpr std::string_view kKeyStoreSubdir = "var/lib/sbctl";

constexpr std::array<uint8_t, 39> kSpcPeImageDataValue = {
    0x30, 0x25, 0x03, 0x01, 0x00, 0xa0, 0x20, 0xa2, 0x1e, 0x80,
    0x1c, 0x00, 0x3c, 0x00, 0x3c, 0x00, 0x3c, 0x00, 0x4f, 0x00,
    0x62, 0x00, 0x73, 0x00, 0x6f, 0x00, 0x6c, 0x00, 0x65, 0x00,
    0x74, 0x00, 0x65, 0x00, 0x3e, 0x00, 0x3e, 0x00, 0x3e};

constexpr std::array<uint8_t, 16> kEfiCertX509Guid = {
    0xa1, 0x59, 0xc0, 0xa5, 0xe4, 0x94, 0xa7, 0x4a,
    0x87, 0xb5, 0xab, 0x15, 0x5c, 0x2b, 0xf0, 0x72};

constexpr std::array<uint8_t, 16> kEfiCertTypePkcs7Guid = {
    0x9d, 0xd2, 0xaf, 0x4a, 0xdf, 0x68, 0xee, 0x49,
    0x8a, 0xa9, 0x34, 0x7d, 0x37, 0x56, 0x65, 0xa7};

template <typename T, auto Free>
struct Deleter {
  void operator()(T* ptr) const { Free(ptr); }
};

template <typename T, auto Free>
using OsslPtr = std::unique_ptr<T, Deleter<T, Free>>;

inline void FreeX509Stack(STACK_OF(X509) * stack) { sk_X509_free(stack); }
inline void CryptoFree(void* ptr) { OPENSSL_free(ptr); }

using BioPtr = OsslPtr<BIO, BIO_free>;
using EvpPkeyPtr = OsslPtr<EVP_PKEY, EVP_PKEY_free>;
using EvpMdCtxPtr = OsslPtr<EVP_MD_CTX, EVP_MD_CTX_free>;
using X509Ptr = OsslPtr<X509, X509_free>;
using X509NamePtr = OsslPtr<X509_NAME, X509_NAME_free>;
using X509StorePtr = OsslPtr<X509_STORE, X509_STORE_free>;
using X509StackPtr = OsslPtr<STACK_OF(X509), FreeX509Stack>;
using Pkcs7Ptr = OsslPtr<PKCS7, PKCS7_free>;
using BignumPtr = OsslPtr<BIGNUM, BN_free>;
using Asn1IntegerPtr = OsslPtr<ASN1_INTEGER, ASN1_INTEGER_free>;
using Asn1TimePtr = OsslPtr<ASN1_TIME, ASN1_TIME_free>;
using OpensslBytesPtr = OsslPtr<uint8_t, CryptoFree>;

Status OpensslError(std::string_view operation) {
  std::array<char, 256> text{};
  ERR_error_string_n(ERR_get_error(), text.data(), text.size());
  ERR_clear_error();
  return InternalError("{} failed: {}", operation, text.data());
}

struct KeyAndCert {
  EvpPkeyPtr key;
  X509Ptr certificate;
};

std::vector<uint8_t> Concat(
    std::initializer_list<std::span<const uint8_t>> parts) {
  size_t total = 0;
  for (const auto& part : parts) {
    total += part.size();
  }
  std::vector<uint8_t> out;
  out.reserve(total);
  for (const auto& part : parts) {
    out.insert(out.end(), part.begin(), part.end());
  }
  return out;
}

std::vector<uint8_t> Bytes(std::span<const uint8_t> data) {
  return {data.begin(), data.end()};
}

std::vector<uint8_t> Octet(std::span<const uint8_t> contents) {
  return der::Tlv(0x04, contents);
}

std::vector<uint8_t> IntegerU8(uint8_t value) {
  return der::Tlv(0x02, std::span<const uint8_t>(&value, 1));
}

StatusOr<std::vector<uint8_t>> AlgorithmIdentifier(std::string_view oid) {
  static constexpr std::array<uint8_t, 2> kDerNull = {0x05, 0x00};
  PUU_ASSIGN_OR_RETURN(const std::vector<uint8_t> encoded, der::Oid(oid));
  return der::Sequence(Concat({encoded, Bytes(kDerNull)}));
}

StatusOr<uint32_t> ReadU32Le(std::span<const uint8_t> data, size_t offset) {
  if (!LeFits<uint32_t>(data, offset)) {
    return InvalidArgumentError("truncated PE at offset {:#x}", offset);
  }
  return ReadLe<uint32_t>(data, offset);
}

StatusOr<uint16_t> ReadU16Le(std::span<const uint8_t> data, size_t offset) {
  if (!LeFits<uint16_t>(data, offset)) {
    return InvalidArgumentError("truncated PE at offset {:#x}", offset);
  }
  return ReadLe<uint16_t>(data, offset);
}

StatusOr<std::span<const uint8_t>> Slice(std::span<const uint8_t> data,
                                         size_t start, size_t end) {
  if (start > end || end > data.size()) {
    return InvalidArgumentError("PE region {:#x}..{:#x} out of bounds", start,
                                end);
  }
  return data.subspan(start, end - start);
}

StatusOr<AuthenticodeLayout> ComputeAuthenticodeLayout(
    std::span<const uint8_t> image) {
  AuthenticodeLayout layout;
  PUU_ASSIGN_OR_RETURN(const uint32_t e_lfanew, ReadU32Le(image, 0x3c));

  layout.optional_header_offset = static_cast<size_t>(e_lfanew) + 24;

  PUU_ASSIGN_OR_RETURN(const uint16_t magic,
                       ReadU16Le(image, layout.optional_header_offset));

  size_t directory_start = 0;
  if (magic == 0x10b) {
    directory_start = layout.optional_header_offset + 96;
  } else if (magic == 0x20b) {
    directory_start = layout.optional_header_offset + 112;
  } else {
    return InvalidArgumentError("unsupported PE optional header magic {:#x}",
                                magic);
  }

  layout.checksum_offset = layout.optional_header_offset + 64;

  layout.security_dir_offset = directory_start + (4 * size_t{8});

  PUU_ASSIGN_OR_RETURN(const uint32_t size_of_headers,
                       ReadU32Le(image, layout.optional_header_offset + 60));
  layout.size_of_headers = size_of_headers;
  PUU_ASSIGN_OR_RETURN(const uint16_t number_of_sections,
                       ReadU16Le(image, static_cast<size_t>(e_lfanew) + 6));
  layout.number_of_sections = number_of_sections;
  PUU_ASSIGN_OR_RETURN(const uint16_t size_of_optional_header,
                       ReadU16Le(image, static_cast<size_t>(e_lfanew) + 20));
  layout.section_table_offset =
      layout.optional_header_offset + size_of_optional_header;

  PUU_ASSIGN_OR_RETURN(const uint32_t table_offset,
                       ReadU32Le(image, layout.security_dir_offset));
  PUU_ASSIGN_OR_RETURN(const uint32_t table_size,
                       ReadU32Le(image, layout.security_dir_offset + 4));
  layout.certificate_table_offset = table_offset;
  layout.certificate_table_size = table_size;

  if (layout.checksum_offset + 4 > layout.security_dir_offset ||
      layout.security_dir_offset + 8 > layout.size_of_headers ||
      layout.size_of_headers > image.size()) {
    return InvalidArgumentError("PE headers are inconsistent or truncated");
  }
  const size_t section_table_end =
      layout.section_table_offset + (layout.number_of_sections * 40);
  if (section_table_end < layout.section_table_offset ||
      section_table_end > image.size()) {
    return InvalidArgumentError("PE section table exceeds image");
  }
  return layout;
}

StatusOr<std::vector<uint8_t>> CertificateDer(const X509* certificate) {
  uint8_t* raw = nullptr;
  const int length = i2d_X509(certificate, &raw);
  if (length <= 0 || raw == nullptr) {
    return OpensslError("i2d_X509");
  }
  const OpensslBytesPtr owned(raw);
  return std::vector<uint8_t>(raw, raw + length);
}

StatusOr<KeyAndCert> LoadKeyAndCert(const KeyPair& pair) {
  PUU_ASSIGN_OR_RETURN(std::vector<uint8_t> key_pem,
                       ReadFileBytes(pair.key_path));
  PUU_ASSIGN_OR_RETURN(const std::vector<uint8_t> cert_pem,
                       ReadFileBytes(pair.certificate_path));

  StatusOr<EvpPkeyPtr> parsed_key = [&key_pem,
                                     &pair]() -> StatusOr<EvpPkeyPtr> {
    const BioPtr key_bio(
        BIO_new_mem_buf(key_pem.data(), static_cast<int>(key_pem.size())));
    if (key_bio == nullptr) {
      return OpensslError("BIO_new_mem_buf");
    }
    EvpPkeyPtr key(
        PEM_read_bio_PrivateKey(key_bio.get(), nullptr, nullptr, nullptr));
    if (key == nullptr) {
      return WithContext(
          OpensslError("PEM_read_bio_PrivateKey"),
          fmt::format("failed to load {}", pair.key_path.string()));
    }
    return key;
  }();
  OPENSSL_cleanse(key_pem.data(), key_pem.size());
  if (!parsed_key.ok()) {
    return parsed_key.status();
  }
  EvpPkeyPtr key = std::move(*parsed_key);

  const BioPtr cert_bio(
      BIO_new_mem_buf(cert_pem.data(), static_cast<int>(cert_pem.size())));
  if (cert_bio == nullptr) {
    return OpensslError("BIO_new_mem_buf");
  }
  X509Ptr certificate(
      PEM_read_bio_X509(cert_bio.get(), nullptr, nullptr, nullptr));
  if (certificate == nullptr) {
    return WithContext(
        OpensslError("PEM_read_bio_X509"),
        fmt::format("failed to load {}", pair.certificate_path.string()));
  }
  return KeyAndCert{.key = std::move(key),
                    .certificate = std::move(certificate)};
}

StatusOr<X509Ptr> LoadCertificate(const std::filesystem::path& path) {
  PUU_ASSIGN_OR_RETURN(const std::vector<uint8_t> pem, ReadFileBytes(path));
  const BioPtr bio(BIO_new_mem_buf(pem.data(), static_cast<int>(pem.size())));
  if (bio == nullptr) {
    return OpensslError("BIO_new_mem_buf");
  }
  X509Ptr certificate(PEM_read_bio_X509(bio.get(), nullptr, nullptr, nullptr));
  if (certificate == nullptr) {
    return WithContext(OpensslError("PEM_read_bio_X509"),
                       fmt::format("failed to load {}", path.string()));
  }
  return certificate;
}

StatusOr<std::pair<std::vector<uint8_t>, std::vector<uint8_t>>> IssuerAndSerial(
    std::span<const uint8_t> cert_der) {
  PUU_ASSIGN_OR_RETURN(const der::Parsed certificate, der::ReadAt(cert_der, 0));
  if (certificate.tag != 0x30) {
    return InvalidArgumentError("certificate is not a SEQUENCE");
  }
  PUU_ASSIGN_OR_RETURN(const der::Parsed tbs,
                       der::ReadAt(certificate.contents, 0));
  if (tbs.tag != 0x30) {
    return InvalidArgumentError("tbsCertificate is not a SEQUENCE");
  }
  const std::span<const uint8_t> body = tbs.contents;

  PUU_ASSIGN_OR_RETURN(const der::Parsed first, der::ReadAt(body, 0));

  const size_t serial_offset = first.tag == 0xa0 ? first.next : 0;
  PUU_ASSIGN_OR_RETURN(const der::Parsed serial,
                       der::ReadAt(body, serial_offset));
  if (serial.tag != 0x02) {
    return InvalidArgumentError("serialNumber is not an INTEGER");
  }
  PUU_ASSIGN_OR_RETURN(const der::Parsed signature,
                       der::ReadAt(body, serial.next));
  PUU_ASSIGN_OR_RETURN(const der::Parsed issuer,
                       der::ReadAt(body, signature.next));
  if (issuer.tag != 0x30) {
    return InvalidArgumentError("issuer is not a Name SEQUENCE");
  }
  return std::make_pair(Bytes(issuer.total), Bytes(serial.total));
}

StatusOr<std::vector<uint8_t>> BuildPkcs7(std::span<const uint8_t> digest,
                                          EVP_PKEY* key,
                                          const X509* certificate) {
  PUU_ASSIGN_OR_RETURN(const std::vector<uint8_t> cert_der,
                       CertificateDer(certificate));
  PUU_ASSIGN_OR_RETURN(const auto issuer_and_serial, IssuerAndSerial(cert_der));

  PUU_ASSIGN_OR_RETURN(const std::vector<uint8_t> oid_pe_image,
                       der::Oid(kOidSpcPeImageData));
  const std::vector<uint8_t> spc_attr =
      der::Sequence(Concat({oid_pe_image, Bytes(kSpcPeImageDataValue)}));
  PUU_ASSIGN_OR_RETURN(const std::vector<uint8_t> sha256_algorithm,
                       AlgorithmIdentifier(kOidSha256));
  const std::vector<uint8_t> spc_digest =
      der::Sequence(Concat({sha256_algorithm, Octet(digest)}));
  const std::vector<uint8_t> spc =
      der::Sequence(Concat({spc_attr, spc_digest}));

  PUU_ASSIGN_OR_RETURN(const der::Parsed spc_tlv, der::ReadAt(spc, 0));
  if (spc_tlv.tag != 0x30 || spc_tlv.next != spc.size()) {
    return InternalError("invalid SpcIndirectDataContent encoding");
  }
  PUU_ASSIGN_OR_RETURN(const auto content_digest, Sha256(spc_tlv.contents));

  PUU_ASSIGN_OR_RETURN(const std::vector<uint8_t> oid_content_type,
                       der::Oid(kOidContentType));
  PUU_ASSIGN_OR_RETURN(const std::vector<uint8_t> oid_indirect_data,
                       der::Oid(kOidSpcIndirectData));
  PUU_ASSIGN_OR_RETURN(const std::vector<uint8_t> oid_message_digest,
                       der::Oid(kOidMessageDigest));
  const std::vector<uint8_t> content_type_attr =
      der::Sequence(Concat({oid_content_type, der::Set(oid_indirect_data)}));
  const std::vector<uint8_t> message_digest_attr = der::Sequence(
      Concat({oid_message_digest, der::Set(Octet(content_digest))}));

  std::array<std::vector<uint8_t>, 2> attributes = {content_type_attr,
                                                    message_digest_attr};
  std::ranges::sort(attributes);
  const std::vector<uint8_t> attribute_contents =
      Concat({attributes[0], attributes[1]});

  const std::vector<uint8_t> to_sign = der::Tlv(0x31, attribute_contents);
  const EvpMdCtxPtr md_ctx(EVP_MD_CTX_new());
  if (md_ctx == nullptr) {
    return OpensslError("EVP_MD_CTX_new");
  }
  if (EVP_DigestSignInit(md_ctx.get(), nullptr, EVP_sha256(), nullptr, key) !=
      1) {
    return OpensslError("EVP_DigestSignInit");
  }
  size_t signature_length = 0;
  if (EVP_DigestSign(md_ctx.get(), nullptr, &signature_length, to_sign.data(),
                     to_sign.size()) != 1) {
    return OpensslError("EVP_DigestSign");
  }
  std::vector<uint8_t> signature(signature_length);
  if (EVP_DigestSign(md_ctx.get(), signature.data(), &signature_length,
                     to_sign.data(), to_sign.size()) != 1) {
    return OpensslError("EVP_DigestSign");
  }
  signature.resize(signature_length);

  PUU_ASSIGN_OR_RETURN(const std::vector<uint8_t> rsa_algorithm,
                       AlgorithmIdentifier(kOidRsaEncryption));
  const std::vector<uint8_t> signer_info = der::Sequence(Concat({
      IntegerU8(1),
      der::Sequence(
          Concat({issuer_and_serial.first, issuer_and_serial.second})),
      sha256_algorithm,
      der::Tlv(0xa0, attribute_contents),
      rsa_algorithm,
      Octet(signature),
  }));

  const std::vector<uint8_t> encap =
      der::Sequence(Concat({oid_indirect_data, der::Tlv(0xa0, spc)}));
  const std::vector<uint8_t> signed_data = der::Sequence(Concat({
      IntegerU8(1),
      der::Set(sha256_algorithm),
      encap,
      der::Tlv(0xa0, cert_der),
      der::Set(signer_info),
  }));

  PUU_ASSIGN_OR_RETURN(const std::vector<uint8_t> oid_signed_data,
                       der::Oid(kOidSignedData));
  return der::Sequence(Concat({oid_signed_data, der::Tlv(0xa0, signed_data)}));
}

StatusOr<std::optional<std::pair<size_t, size_t>>> CertificateTableRange(
    std::span<const uint8_t> image) {
  PUU_ASSIGN_OR_RETURN(const AuthenticodeLayout layout,
                       ComputeAuthenticodeLayout(image));
  const size_t table_offset = layout.certificate_table_offset;
  const size_t table_size = layout.certificate_table_size;
  if (table_offset == 0 && table_size == 0) {
    return std::optional<std::pair<size_t, size_t>>{};
  }
  if (table_offset == 0 || table_size == 0) {
    return InvalidArgumentError("incomplete PE certificate table directory");
  }
  if (table_offset % 8 != 0) {
    return InvalidArgumentError("PE certificate table is not 8-byte aligned");
  }
  const size_t table_end = table_offset + table_size;
  if (table_end < table_offset) {
    return InvalidArgumentError("PE certificate table range overflows");
  }

  if (table_end != image.size()) {
    return InvalidArgumentError(
        "PE certificate table must be the final "
        "image region");
  }

  size_t image_data_end = layout.size_of_headers;
  for (size_t index = 0; index < layout.number_of_sections; ++index) {
    const size_t header = layout.section_table_offset + (index * 40);
    PUU_ASSIGN_OR_RETURN(const uint32_t start, ReadU32Le(image, header + 20));
    PUU_ASSIGN_OR_RETURN(const uint32_t size, ReadU32Le(image, header + 16));
    const size_t end = static_cast<size_t>(start) + size;
    if (end > image.size()) {
      return InvalidArgumentError("PE section exceeds image");
    }
    image_data_end = std::max(image_data_end, end);
  }
  if (table_offset < image_data_end) {
    return InvalidArgumentError(
        "PE certificate table overlaps headers or section data");
  }
  return std::optional<std::pair<size_t, size_t>>{{table_offset, table_end}};
}

StatusOr<std::array<uint8_t, 32>> AuthenticodeSha256(
    std::span<const uint8_t> image) {
  PUU_ASSIGN_OR_RETURN(const AuthenticodeLayout layout,
                       ComputeAuthenticodeLayout(image));

  const EvpMdCtxPtr ctx(EVP_MD_CTX_new());
  if (ctx == nullptr) {
    return OpensslError("EVP_MD_CTX_new");
  }
  if (EVP_DigestInit_ex(ctx.get(), EVP_sha256(), nullptr) != 1) {
    return OpensslError("EVP_DigestInit_ex");
  }
  const auto update = [&ctx](std::span<const uint8_t> data) -> Status {
    if (data.empty()) {
      return absl::OkStatus();
    }
    if (EVP_DigestUpdate(ctx.get(), data.data(), data.size()) != 1) {
      return OpensslError("EVP_DigestUpdate");
    }
    return absl::OkStatus();
  };

  PUU_ASSIGN_OR_RETURN(const std::span<const uint8_t> head,
                       Slice(image, 0, layout.checksum_offset));
  PUU_RETURN_IF_ERROR(update(head));
  PUU_ASSIGN_OR_RETURN(
      const std::span<const uint8_t> between,
      Slice(image, layout.checksum_offset + 4, layout.security_dir_offset));
  PUU_RETURN_IF_ERROR(update(between));
  PUU_ASSIGN_OR_RETURN(
      const std::span<const uint8_t> rest_of_headers,
      Slice(image, layout.security_dir_offset + 8, layout.size_of_headers));
  PUU_RETURN_IF_ERROR(update(rest_of_headers));

  std::vector<std::pair<size_t, size_t>> ranges;
  ranges.reserve(layout.number_of_sections);
  for (size_t index = 0; index < layout.number_of_sections; ++index) {
    const size_t header = layout.section_table_offset + (index * 40);
    PUU_ASSIGN_OR_RETURN(const uint32_t pointer, ReadU32Le(image, header + 20));
    PUU_ASSIGN_OR_RETURN(const uint32_t size, ReadU32Le(image, header + 16));
    ranges.emplace_back(pointer, static_cast<size_t>(pointer) + size);
  }
  std::ranges::sort(ranges, [](const auto& lhs, const auto& rhs) {
    return lhs.first < rhs.first;
  });

  size_t bytes_hashed = layout.size_of_headers;
  for (const auto& [start, end] : ranges) {
    PUU_ASSIGN_OR_RETURN(const std::span<const uint8_t> section,
                         Slice(image, start, end));
    PUU_RETURN_IF_ERROR(update(section));
    bytes_hashed += end - start;
  }

  if (image.size() < bytes_hashed ||
      image.size() - bytes_hashed < layout.certificate_table_size) {
    return InvalidArgumentError(
        "invalid PE layout while hashing trailing "
        "data");
  }
  const size_t extra =
      image.size() - bytes_hashed - layout.certificate_table_size;
  PUU_ASSIGN_OR_RETURN(const std::span<const uint8_t> trailing,
                       Slice(image, bytes_hashed, bytes_hashed + extra));
  PUU_RETURN_IF_ERROR(update(trailing));

  std::array<uint8_t, 32> digest{};
  unsigned length = 0;
  if (EVP_DigestFinal_ex(ctx.get(), digest.data(), &length) != 1) {
    return OpensslError("EVP_DigestFinal_ex");
  }
  return digest;
}

StatusOr<std::vector<std::span<const uint8_t>>> AttributeCertificates(
    std::span<const uint8_t> image) {
  PUU_ASSIGN_OR_RETURN(const auto table, CertificateTableRange(image));
  std::vector<std::span<const uint8_t>> out;
  if (!table.has_value()) {
    return out;
  }

  const auto [table_start, table_end] = *table;
  size_t position = table_start;
  while (position < table_end) {
    if (table_end - position < 8) {
      return InvalidArgumentError("truncated certificate entry header");
    }
    PUU_ASSIGN_OR_RETURN(const uint32_t entry_size, ReadU32Le(image, position));
    PUU_ASSIGN_OR_RETURN(const uint16_t revision,
                         ReadU16Le(image, position + 4));
    PUU_ASSIGN_OR_RETURN(const uint16_t type, ReadU16Le(image, position + 6));
    if (entry_size < 8) {
      return InvalidArgumentError("invalid certificate entry size {}",
                                  entry_size);
    }
    const size_t entry_end = position + entry_size;
    if (entry_end > table_end) {
      return InvalidArgumentError("certificate entry exceeds table");
    }
    if (revision == kWinCertRevision20 && type == kWinCertTypePkcsSignedData) {
      out.push_back(image.subspan(position + 8, entry_size - 8));
    }
    position += AlignCertificateEntry(entry_size);
    if (position > table_end) {
      return InvalidArgumentError("certificate entry padding exceeds table");
    }
  }
  return out;
}

StatusOr<std::vector<uint8_t>> SignPeImage(std::span<const uint8_t> image,
                                           const KeyAndCert& signer) {
  const EvpPkeyPtr public_key(X509_get_pubkey(signer.certificate.get()));
  if (public_key == nullptr) {
    return OpensslError("X509_get_pubkey");
  }
  if (EVP_PKEY_eq(public_key.get(), signer.key.get()) != 1) {
    return FailedPreconditionError(
        "Secure Boot private key does not match its certificate");
  }

  PUU_ASSIGN_OR_RETURN(const AuthenticodeLayout layout,
                       ComputeAuthenticodeLayout(image));
  PUU_ASSIGN_OR_RETURN(const auto table, CertificateTableRange(image));

  std::vector<uint8_t> out(image.begin(), image.end());
  if (table.has_value()) {
    out.resize(table->first);
  }
  std::fill_n(
      out.begin() + static_cast<std::ptrdiff_t>(layout.security_dir_offset), 8,
      uint8_t{0});

  while (out.size() % 8 != 0) {
    out.push_back(0);
  }

  PUU_ASSIGN_OR_RETURN(const auto digest, AuthenticodeSha256(out));
  PUU_ASSIGN_OR_RETURN(
      const std::vector<uint8_t> pkcs7,
      BuildPkcs7(digest, signer.key.get(), signer.certificate.get()));

  const size_t entry_length = 8 + pkcs7.size();
  if (entry_length > 0xffff'ffffU) {
    return InvalidArgumentError(
        "certificate entry is too large for a PE "
        "image");
  }
  const size_t padded_length = AlignCertificateEntry(entry_length);
  std::vector<uint8_t> entry;
  entry.reserve(padded_length);
  AppendLe<uint32_t>(entry, static_cast<uint32_t>(entry_length));
  AppendLe<uint16_t>(entry, kWinCertRevision20);
  AppendLe<uint16_t>(entry, kWinCertTypePkcsSignedData);
  entry.insert(entry.end(), pkcs7.begin(), pkcs7.end());
  entry.resize(padded_length, 0);

  const size_t certificate_offset = out.size();
  if (certificate_offset > 0xffff'ffffU) {
    return InvalidArgumentError(
        "PE image is too large for a certificate "
        "table");
  }
  out.insert(out.end(), entry.begin(), entry.end());

  std::vector<uint8_t> directory;
  AppendLe<uint32_t>(directory, static_cast<uint32_t>(certificate_offset));
  AppendLe<uint32_t>(directory, static_cast<uint32_t>(entry.size()));
  std::ranges::copy(directory, out.begin() + static_cast<std::ptrdiff_t>(
                                                 layout.security_dir_offset));
  return out;
}

StatusOr<bool> VerifyPeImage(std::span<const uint8_t> image,
                             X509* certificate) {
  PUU_ASSIGN_OR_RETURN(const std::vector<uint8_t> cert_der,
                       CertificateDer(certificate));
  PUU_ASSIGN_OR_RETURN(const auto expected, AuthenticodeSha256(image));

  const X509StackPtr certificates(sk_X509_new_null());
  if (certificates == nullptr) {
    return OpensslError("sk_X509_new_null");
  }
  if (sk_X509_push(certificates.get(), certificate) <= 0) {
    return OpensslError("sk_X509_push");
  }
  const X509StorePtr store(X509_STORE_new());
  if (store == nullptr) {
    return OpensslError("X509_STORE_new");
  }

  const int flags = PKCS7_BINARY | PKCS7_NOINTERN | PKCS7_NOVERIFY;
  PUU_ASSIGN_OR_RETURN(const std::vector<std::span<const uint8_t>> blobs,
                       AttributeCertificates(image));
  for (const std::span<const uint8_t> blob : blobs) {
    const std::optional<der::EmbeddedSignature> parsed =
        der::ParseAuthenticode(blob);
    if (!parsed.has_value()) {
      continue;
    }
    if (!std::equal(parsed->digest.begin(), parsed->digest.end(),
                    expected.begin(), expected.end())) {
      continue;
    }
    if (std::find(parsed->certificates.begin(), parsed->certificates.end(),
                  cert_der) == parsed->certificates.end()) {
      continue;
    }
    const uint8_t* cursor = blob.data();
    const Pkcs7Ptr pkcs7(
        d2i_PKCS7(nullptr, &cursor, static_cast<int64_t>(blob.size())));
    if (pkcs7 == nullptr) {
      ERR_clear_error();
      continue;
    }
    if (PKCS7_verify(pkcs7.get(), certificates.get(), store.get(), nullptr,
                     nullptr, flags) == 1) {
      return true;
    }
    ERR_clear_error();
  }
  return false;
}

std::vector<uint8_t> AuthMessage(std::string_view name, const EfiGuid& guid,
                                 std::span<const uint8_t> data) {
  std::vector<uint8_t> message;

  for (const char c : name) {
    AppendLe<uint16_t>(message, static_cast<uint16_t>(static_cast<uint8_t>(c)));
  }
  message.insert(message.end(), guid.Bytes().begin(), guid.Bytes().end());
  AppendLe<uint32_t>(message, kAuthVariableAttributes);

  message.insert(message.end(), 16, 0);
  message.insert(message.end(), data.begin(), data.end());
  return message;
}

StatusOr<std::vector<uint8_t>> SignDetachedPkcs7(
    std::span<const uint8_t> message, EVP_PKEY* key, X509* certificate) {
  const BioPtr bio(
      BIO_new_mem_buf(message.data(), static_cast<int>(message.size())));
  if (bio == nullptr) {
    return OpensslError("BIO_new_mem_buf");
  }
  const X509StackPtr certificates(sk_X509_new_null());
  if (certificates == nullptr) {
    return OpensslError("sk_X509_new_null");
  }
  const Pkcs7Ptr pkcs7(
      PKCS7_sign(certificate, key, certificates.get(), bio.get(),
                 PKCS7_DETACHED | PKCS7_BINARY | PKCS7_NOATTR));
  if (pkcs7 == nullptr) {
    return OpensslError("PKCS7_sign");
  }
  uint8_t* raw = nullptr;
  const int length = i2d_PKCS7(pkcs7.get(), &raw);
  if (length <= 0 || raw == nullptr) {
    return OpensslError("i2d_PKCS7");
  }
  const OpensslBytesPtr owned(raw);
  return std::vector<uint8_t>(raw, raw + length);
}

StatusOr<std::vector<uint8_t>> AuthenticatedPayload(
    std::string_view name, const EfiGuid& guid,
    std::span<const uint8_t> signature_list, EVP_PKEY* key, X509* certificate) {
  const std::vector<uint8_t> message = AuthMessage(name, guid, signature_list);
  PUU_ASSIGN_OR_RETURN(const std::vector<uint8_t> pkcs7,
                       SignDetachedPkcs7(message, key, certificate));

  std::vector<uint8_t> out;
  out.insert(out.end(), 16, 0);
  const size_t win_cert_length = 8 + 16 + pkcs7.size();
  if (win_cert_length > 0xffff'ffffU) {
    return InternalError("authenticated variable signature is too large");
  }
  AppendLe<uint32_t>(out, static_cast<uint32_t>(win_cert_length));
  AppendLe<uint16_t>(out, kWinCertRevision20);
  AppendLe<uint16_t>(out, kWinCertTypeEfiGuid);
  out.insert(out.end(), kEfiCertTypePkcs7Guid.begin(),
             kEfiCertTypePkcs7Guid.end());
  out.insert(out.end(), pkcs7.begin(), pkcs7.end());
  out.insert(out.end(), signature_list.begin(), signature_list.end());
  return out;
}

Status WriteAuthVariable(std::string_view name, const EfiGuid& guid,
                         std::span<const uint8_t> payload) {
  return WriteEfiVariable(EfiVariablePath(name, guid), kAuthVariableAttributes,
                          payload);
}

StatusOr<std::string> RandomUuidText() {
  std::array<uint8_t, 16> bytes{};
  PUU_RETURN_IF_ERROR(RandomBytes(bytes));

  bytes[6] = static_cast<uint8_t>((bytes[6] & 0x0fU) | 0x40U);
  bytes[8] = static_cast<uint8_t>((bytes[8] & 0x3fU) | 0x80U);
  std::string text;
  for (size_t i = 0; i < bytes.size(); ++i) {
    if (i == 4 || i == 6 || i == 8 || i == 10) {
      text.push_back('-');
    }
    text += fmt::format("{:02x}", bytes[i]);
  }
  return text;
}

StatusOr<EfiGuid> ParseEfiGuid(std::string_view text) {
  std::array<uint8_t, 16> big_endian{};
  size_t nibble = 0;
  for (const char c : text) {
    if (c == '-') {
      continue;
    }
    uint8_t value = 0;
    if (c >= '0' && c <= '9') {
      value = static_cast<uint8_t>(c - '0');
    } else if (c >= 'a' && c <= 'f') {
      value = static_cast<uint8_t>(c - 'a' + 10);
    } else if (c >= 'A' && c <= 'F') {
      value = static_cast<uint8_t>(c - 'A' + 10);
    } else {
      return InvalidArgumentError("invalid character in UUID '{}'", text);
    }
    if (nibble >= 32) {
      return InvalidArgumentError("UUID '{}' is too long", text);
    }
    if (nibble % 2 == 0) {
      big_endian[nibble / 2] = static_cast<uint8_t>(value << 4U);
    } else {
      big_endian[nibble / 2] =
          static_cast<uint8_t>(big_endian[nibble / 2] | value);
    }
    ++nibble;
  }
  if (nibble != 32) {
    return InvalidArgumentError("UUID '{}' is too short", text);
  }

  std::array<uint8_t, 16> mixed{};
  mixed[0] = big_endian[3];
  mixed[1] = big_endian[2];
  mixed[2] = big_endian[1];
  mixed[3] = big_endian[0];
  mixed[4] = big_endian[5];
  mixed[5] = big_endian[4];
  mixed[6] = big_endian[7];
  mixed[7] = big_endian[6];
  std::copy(big_endian.begin() + 8, big_endian.end(), mixed.begin() + 8);
  return EfiGuid(mixed);
}

Status GenerateKeyPair(std::string_view common_name,
                       const std::filesystem::path& key_path,
                       const std::filesystem::path& certificate_path) {
  const EvpPkeyPtr key(EVP_RSA_gen(2048));
  if (key == nullptr) {
    return OpensslError("EVP_RSA_gen");
  }

  const X509NamePtr name(X509_NAME_new());
  if (name == nullptr) {
    return OpensslError("X509_NAME_new");
  }
  const std::string cn{common_name};
  if (X509_NAME_add_entry_by_txt(
          name.get(), "CN", MBSTRING_ASC,
          reinterpret_cast<const unsigned char*>(cn.c_str()), -1, -1, 0) != 1) {
    return OpensslError("X509_NAME_add_entry_by_txt");
  }

  const X509Ptr certificate(X509_new());
  if (certificate == nullptr) {
    return OpensslError("X509_new");
  }

  if (X509_set_version(certificate.get(), 2) != 1) {
    return OpensslError("X509_set_version");
  }

  const BignumPtr serial(BN_new());
  if (serial == nullptr) {
    return OpensslError("BN_new");
  }

  if (BN_rand(serial.get(), 159, BN_RAND_TOP_ANY, BN_RAND_BOTTOM_ANY) != 1) {
    return OpensslError("BN_rand");
  }
  const Asn1IntegerPtr serial_asn1(BN_to_ASN1_INTEGER(serial.get(), nullptr));
  if (serial_asn1 == nullptr) {
    return OpensslError("BN_to_ASN1_INTEGER");
  }
  if (X509_set_serialNumber(certificate.get(), serial_asn1.get()) != 1) {
    return OpensslError("X509_set_serialNumber");
  }

  if (X509_set_subject_name(certificate.get(), name.get()) != 1 ||
      X509_set_issuer_name(certificate.get(), name.get()) != 1) {
    return OpensslError("X509_set_subject_name");
  }
  if (X509_set_pubkey(certificate.get(), key.get()) != 1) {
    return OpensslError("X509_set_pubkey");
  }

  const Asn1TimePtr not_before(ASN1_TIME_adj(nullptr, time(nullptr), 0, 0));
  const Asn1TimePtr not_after(ASN1_TIME_adj(nullptr, time(nullptr), 3650, 0));
  if (not_before == nullptr || not_after == nullptr) {
    return OpensslError("ASN1_TIME_adj");
  }
  if (X509_set1_notBefore(certificate.get(), not_before.get()) != 1 ||
      X509_set1_notAfter(certificate.get(), not_after.get()) != 1) {
    return OpensslError("X509_set1_notBefore");
  }
  if (X509_sign(certificate.get(), key.get(), EVP_sha256()) == 0) {
    return OpensslError("X509_sign");
  }

  const BioPtr key_bio(BIO_new(BIO_s_mem()));
  if (key_bio == nullptr) {
    return OpensslError("BIO_new");
  }
  if (PEM_write_bio_PKCS8PrivateKey(key_bio.get(), key.get(), nullptr, nullptr,
                                    0, nullptr, nullptr) != 1) {
    return OpensslError("PEM_write_bio_PKCS8PrivateKey");
  }
  const BioPtr cert_bio(BIO_new(BIO_s_mem()));
  if (cert_bio == nullptr) {
    return OpensslError("BIO_new");
  }
  if (PEM_write_bio_X509(cert_bio.get(), certificate.get()) != 1) {
    return OpensslError("PEM_write_bio_X509");
  }

  const auto bio_bytes = [](BIO* bio) -> std::span<uint8_t> {
    char* data = nullptr;
    const auto length = static_cast<int64_t>(BIO_get_mem_data(bio, &data));
    if (data == nullptr || length <= 0) {
      return {};
    }
    return {reinterpret_cast<uint8_t*>(data), static_cast<size_t>(length)};
  };
  const std::span<uint8_t> key_pem = bio_bytes(key_bio.get());
  const std::span<uint8_t> cert_pem = bio_bytes(cert_bio.get());
  Status written;
  if (key_pem.empty() || cert_pem.empty()) {
    written =
        InternalError("failed to serialise generated {} key", common_name);
  } else {
    written = WriteBytes(key_path, key_pem, 0600);
    if (written.ok()) {
      written = WriteBytes(certificate_path, cert_pem, 0644);
    }
  }

  if (!key_pem.empty()) {
    OPENSSL_cleanse(key_pem.data(), key_pem.size());
  }
  return written;
}

size_t AlignCertificateEntry(size_t size) { return (size + 7) & ~size_t{7}; }

[[maybe_unused]] StatusOr<std::vector<std::pair<size_t, size_t>>>
AuthenticodeSkippedRanges(std::span<const uint8_t> image) {
  PUU_ASSIGN_OR_RETURN(const AuthenticodeLayout layout,
                       ComputeAuthenticodeLayout(image));
  std::vector<std::pair<size_t, size_t>> ranges;
  ranges.emplace_back(layout.checksum_offset, layout.checksum_offset + 4);
  ranges.emplace_back(layout.security_dir_offset,
                      layout.security_dir_offset + 8);

  if (layout.certificate_table_size != 0) {
    ranges.emplace_back(
        layout.certificate_table_offset,
        layout.certificate_table_offset + layout.certificate_table_size);
  }
  return ranges;
}

std::vector<uint8_t> BuildSignatureList(std::span<const uint8_t> cert_der,
                                        const EfiGuid& owner) {
  const size_t signature_size = 16 + cert_der.size();
  const size_t list_size = 28 + signature_size;
  std::vector<uint8_t> out;
  out.reserve(list_size);
  out.insert(out.end(), kEfiCertX509Guid.begin(), kEfiCertX509Guid.end());
  AppendLe<uint32_t>(out, static_cast<uint32_t>(list_size));
  AppendLe<uint32_t>(out, 0);
  AppendLe<uint32_t>(out, static_cast<uint32_t>(signature_size));
  out.insert(out.end(), owner.Bytes().begin(), owner.Bytes().end());
  out.insert(out.end(), cert_der.begin(), cert_der.end());
  return out;
}

}  // namespace

std::string_view KeyKindName(KeyKind kind) {
  switch (kind) {
    case KeyKind::kPlatform:
      return "PK";
    case KeyKind::kKeyExchange:
      return "KEK";
    case KeyKind::kDatabase:
      return "db";
  }
  return "";
}

KeyStore::KeyStore(std::filesystem::path root) : root_(std::move(root)) {}

StatusOr<KeyPair> KeyStore::Get(KeyKind kind) const {
  const std::string name{KeyKindName(kind)};
  const std::filesystem::path directory =
      root_ / kKeyStoreSubdir / "keys" / name;
  KeyPair pair{.key_path = directory / fmt::format("{}.key", name),
               .certificate_path = directory / fmt::format("{}.pem", name)};
  std::error_code error;
  if (!std::filesystem::is_regular_file(pair.key_path, error) ||
      !std::filesystem::is_regular_file(pair.certificate_path, error)) {
    return NotFoundError("Secure Boot {} key material is missing under {}",
                         name, directory.string());
  }
  return pair;
}

StatusOr<EfiGuid> KeyStore::OwnerGuid() const {
  const std::filesystem::path path = root_ / kKeyStoreSubdir / "GUID";
  PUU_ASSIGN_OR_RETURN(const std::vector<uint8_t> raw, ReadFileBytes(path));
  const std::string text(raw.begin(), raw.end());
  const size_t first = text.find_first_not_of(" \t\r\n");
  const size_t last = text.find_last_not_of(" \t\r\n");
  if (first == std::string::npos) {
    return InvalidArgumentError("{} is empty", path.string());
  }
  StatusOr<EfiGuid> guid = ParseEfiGuid(text.substr(first, last - first + 1));
  if (!guid.ok()) {
    return WithContext(guid.status(),
                       fmt::format("failed to read {}", path.string()));
  }
  return *guid;
}

Status KeyStore::EnsureKeys() {
  static constexpr std::array<KeyKind, 3> kKinds = {
      KeyKind::kPlatform, KeyKind::kKeyExchange, KeyKind::kDatabase};
  const std::filesystem::path base = root_ / kKeyStoreSubdir;
  const std::filesystem::path key_root = base / "keys";

  std::error_code error;
  bool complete = true;
  bool any_present = false;
  for (const KeyKind kind : kKinds) {
    const std::string name{KeyKindName(kind)};
    const std::filesystem::path directory = key_root / name;

    const bool has_key = std::filesystem::is_regular_file(
        directory / fmt::format("{}.key", name), error);
    const bool has_certificate = std::filesystem::is_regular_file(
        directory / fmt::format("{}.pem", name), error);
    complete = complete && has_key && has_certificate;
    any_present = any_present || has_key || has_certificate;
  }

  if (!complete && any_present) {
    return FailedPreconditionError(
        "Secure Boot key material under {} is incomplete; refusing to "
        "regenerate keys that firmware may already have enrolled",
        base.string());
  }

  if (!complete) {
    const std::filesystem::path staging = base / "keys.staging";
    std::filesystem::remove_all(staging, error);
    if (error) {
      return InternalError("failed to remove {}: {}", staging.string(),
                           error.message());
    }
    for (const KeyKind kind : kKinds) {
      const std::string name{KeyKindName(kind)};
      const std::filesystem::path directory = staging / name;
      std::filesystem::create_directories(directory, error);
      if (error) {
        return InternalError("failed to create {}: {}", directory.string(),
                             error.message());
      }
      PUU_RETURN_IF_ERROR(WithContext(
          GenerateKeyPair(name, directory / fmt::format("{}.key", name),
                          directory / fmt::format("{}.pem", name)),
          fmt::format("failed to generate the Secure Boot {} key", name)));
    }

    if (std::filesystem::exists(key_root, error)) {
      std::filesystem::remove(key_root, error);
      if (error) {
        return InternalError("failed to remove {}: {}", key_root.string(),
                             error.message());
      }
    }
    std::filesystem::rename(staging, key_root, error);
    if (error) {
      return InternalError("failed to publish {}: {}", key_root.string(),
                           error.message());
    }
  }

  std::filesystem::create_directories(base, error);
  if (error) {
    return InternalError("failed to create {}: {}", base.string(),
                         error.message());
  }
  const std::filesystem::path guid_path = base / "GUID";
  if (!std::filesystem::exists(guid_path, error)) {
    PUU_ASSIGN_OR_RETURN(const std::string uuid, RandomUuidText());
    const std::string line = fmt::format("{}\n", uuid);
    PUU_RETURN_IF_ERROR(WriteBytes(
        guid_path, {reinterpret_cast<const uint8_t*>(line.data()), line.size()},
        0644));
  }
  return absl::OkStatus();
}

Status SignPeBinary(const std::filesystem::path& binary, const KeyPair& key) {
  PUU_ASSIGN_OR_RETURN(const std::vector<uint8_t> image, ReadFileBytes(binary));
  PUU_ASSIGN_OR_RETURN(const KeyAndCert signer, LoadKeyAndCert(key));
  StatusOr<std::vector<uint8_t>> signed_image = SignPeImage(image, signer);
  if (!signed_image.ok()) {
    return WithContext(signed_image.status(),
                       fmt::format("failed to sign {}", binary.string()));
  }
  return ReplaceFileBytes(binary, *signed_image);
}

StatusOr<bool> VerifyPeBinary(const std::filesystem::path& binary,
                              const KeyPair& key) {
  PUU_ASSIGN_OR_RETURN(const std::vector<uint8_t> image, ReadFileBytes(binary));
  StatusOr<X509Ptr> certificate = LoadCertificate(key.certificate_path);

  if (!certificate.ok()) {
    return false;
  }
  StatusOr<bool> verified = VerifyPeImage(image, certificate->get());
  if (!verified.ok()) {
    return WithContext(
        verified.status(),
        fmt::format("failed to verify signature for {}", binary.string()));
  }
  return *verified;
}

StatusOr<std::vector<std::filesystem::path>> TargetEfiBinaries(
    const std::filesystem::path& esp) {
  std::vector<std::filesystem::path> found;
  std::error_code error;
  std::filesystem::recursive_directory_iterator it(esp, error);
  if (error) {
    return InternalError("failed to read {}: {}", esp.string(),
                         error.message());
  }
  const std::filesystem::recursive_directory_iterator end;
  for (; it != end; it.increment(error)) {
    if (error) {
      return InternalError("failed to walk {}: {}", esp.string(),
                           error.message());
    }
    std::string extension = it->path().extension().string();
    std::ranges::transform(extension, extension.begin(), [](unsigned char c) {
      return static_cast<char>(std::tolower(c));
    });

    const std::string name = it->path().filename().string();
    if (extension != ".efi" && !name.starts_with("vmlinuz")) {
      continue;
    }
    if (!std::filesystem::is_regular_file(it->path(), error)) {
      continue;
    }
    found.push_back(it->path());
  }
  std::ranges::sort(found);
  return found;
}

namespace {

StatusOr<std::vector<std::vector<uint8_t>>> SplitSignatureLists(
    std::span<const uint8_t> payload) {
  std::vector<std::vector<uint8_t>> lists;
  size_t offset = 0;
  while (offset + 28 <= payload.size()) {
    PUU_ASSIGN_OR_RETURN(const uint32_t list_size,
                         ReadU32Le(payload, offset + 16));
    if (list_size < 28 || list_size > payload.size() - offset) {
      return InternalError("malformed EFI_SIGNATURE_LIST in firmware database");
    }
    lists.emplace_back(
        payload.begin() + static_cast<ptrdiff_t>(offset),
        payload.begin() + static_cast<ptrdiff_t>(offset) + list_size);
    offset += list_size;
  }
  if (offset != payload.size()) {
    return InternalError("trailing bytes in firmware signature database");
  }
  return lists;
}

}  // namespace

Status EnrollKeys(const KeyStore& store) {
  PUU_ASSIGN_OR_RETURN(const EfiGuid owner, store.OwnerGuid());
  PUU_ASSIGN_OR_RETURN(const KeyPair pk_paths, store.Get(KeyKind::kPlatform));
  PUU_ASSIGN_OR_RETURN(const KeyPair kek_paths,
                       store.Get(KeyKind::kKeyExchange));
  PUU_ASSIGN_OR_RETURN(const KeyPair db_paths, store.Get(KeyKind::kDatabase));
  PUU_ASSIGN_OR_RETURN(const KeyAndCert pk, LoadKeyAndCert(pk_paths));
  PUU_ASSIGN_OR_RETURN(const KeyAndCert kek, LoadKeyAndCert(kek_paths));
  PUU_ASSIGN_OR_RETURN(const KeyAndCert db, LoadKeyAndCert(db_paths));

  PUU_ASSIGN_OR_RETURN(const std::vector<uint8_t> pk_der,
                       CertificateDer(pk.certificate.get()));
  PUU_ASSIGN_OR_RETURN(const std::vector<uint8_t> kek_der,
                       CertificateDer(kek.certificate.get()));
  PUU_ASSIGN_OR_RETURN(const std::vector<uint8_t> db_der,
                       CertificateDer(db.certificate.get()));

  const EfiGuid global = EfiGuid::Global();
  const EfiGuid image_security_db = EfiGuid::ImageSecurityDatabase();

  {
    std::vector<uint8_t> esl = BuildSignatureList(db_der, owner);
    const std::filesystem::path current_path =
        EfiVariablePath("db", image_security_db);
    std::error_code exists_error;
    if (std::filesystem::exists(current_path, exists_error)) {
      PUU_ASSIGN_OR_RETURN(const std::vector<uint8_t> raw,
                           ReadFileBytes(current_path));

      if (raw.size() >= 4) {
        PUU_ASSIGN_OR_RETURN(
            const std::vector<std::vector<uint8_t>> existing,
            SplitSignatureLists(std::span<const uint8_t>(raw).subspan(4)));
        for (const std::vector<uint8_t>& list : existing) {
          esl.insert(esl.end(), list.begin(), list.end());
        }
      }
    }
    PUU_ASSIGN_OR_RETURN(
        const std::vector<uint8_t> payload,
        AuthenticatedPayload("db", image_security_db, esl, kek.key.get(),
                             kek.certificate.get()));
    PUU_RETURN_IF_ERROR(
        WithContext(WriteAuthVariable("db", image_security_db, payload),
                    "failed to enrol the Secure Boot signature database"));
  }

  {
    const std::vector<uint8_t> esl = BuildSignatureList(kek_der, owner);
    PUU_ASSIGN_OR_RETURN(const std::vector<uint8_t> payload,
                         AuthenticatedPayload("KEK", global, esl, pk.key.get(),
                                              pk.certificate.get()));
    PUU_RETURN_IF_ERROR(WithContext(WriteAuthVariable("KEK", global, payload),
                                    "failed to enrol the key exchange key"));
  }

  {
    const std::vector<uint8_t> esl = BuildSignatureList(pk_der, owner);
    PUU_ASSIGN_OR_RETURN(const std::vector<uint8_t> payload,
                         AuthenticatedPayload("PK", global, esl, pk.key.get(),
                                              pk.certificate.get()));
    PUU_RETURN_IF_ERROR(WithContext(WriteAuthVariable("PK", global, payload),
                                    "failed to enrol the platform key"));
  }
  return absl::OkStatus();
}

}  // namespace puu::engine
