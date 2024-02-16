#ifndef CLP_S_FFI_IR_STREAM_PROTOCOL_CONSTANTS_HPP
#define CLP_S_FFI_IR_STREAM_PROTOCOL_CONSTANTS_HPP

#include <cstddef>
#include <cstdint>
#include <type_traits>

namespace clp_s::ffi::ir_stream::cProtocol {
using encoded_tag_t = int8_t;

namespace Metadata {
constexpr encoded_tag_t EncodingJson = 0x1;
constexpr encoded_tag_t LengthUByte = 0x11;
constexpr encoded_tag_t LengthUShort = 0x12;

constexpr char VersionKey[] = "VERSION";
constexpr char VersionValue[] = "0.0.1";

// The following regex can be used to validate a Semantic Versioning string.
// The source of the regex can be found here: https://semver.org/
constexpr char VersionRegex[] = "^(0|[1-9]\\d*)\\.(0|[1-9]\\d*)\\.(0|[1-9]\\d*)"
                                "(?:-((?:0|[1-9]\\d*|\\d*[a-zA-Z-][0-9a-zA-Z-]*)"
                                "(?:\\.(?:0|[1-9]\\d*|\\d*[a-zA-Z-][0-9a-zA-Z-]*))*))?"
                                "(?:\\+([0-9a-zA-Z-]+(?:\\.[0-9a-zA-Z-]+)*))?$";

constexpr char TimestampPatternKey[] = "TIMESTAMP_PATTERN";
constexpr char TimestampPatternSyntaxKey[] = "TIMESTAMP_PATTERN_SYNTAX";
constexpr char TimeZoneIdKey[] = "TZ_ID";
constexpr char ReferenceTimestampKey[] = "REFERENCE_TIMESTAMP";

constexpr char VariablesSchemaIdKey[] = "VARIABLES_SCHEMA_ID";
constexpr char VariableEncodingMethodsIdKey[] = "VARIABLE_ENCODING_METHODS_ID";
}  // namespace Metadata

namespace Tag {
constexpr encoded_tag_t ValueInt8{0x51};
constexpr encoded_tag_t ValueInt16{0x52};
constexpr encoded_tag_t ValueInt32{0x53};
constexpr encoded_tag_t ValueInt64{0x54};
constexpr encoded_tag_t ValueDouble{0x55};
constexpr encoded_tag_t ValueTrue{0x56};
constexpr encoded_tag_t ValueFalse{0x57};

constexpr encoded_tag_t ValueStrCLPFourByte{0x58};
constexpr encoded_tag_t ValueStrCLPEightByte{0x59};

constexpr encoded_tag_t ValueEmpty{0x5e};
constexpr encoded_tag_t ValueNull{0x5f};

constexpr encoded_tag_t StandardStrLenByte{0x41};
constexpr encoded_tag_t StandardStrLenShort{0x42};
constexpr encoded_tag_t StandardStrLenInt{0x43};

constexpr encoded_tag_t SchemaNodeParentIdByte{0x60};
constexpr encoded_tag_t SchemaNodeParentIdShort{0x61};

constexpr encoded_tag_t KeyIdByte{0x65};
constexpr encoded_tag_t KeyIdShort{0x66};

constexpr encoded_tag_t SchemaNodeUnknown{0x70};
constexpr encoded_tag_t SchemaNodeInt{0x71};
constexpr encoded_tag_t SchemaNodeFloat{0x72};
constexpr encoded_tag_t SchemaNodeBool{0x73};
constexpr encoded_tag_t SchemaNodeStr{0x74};
constexpr encoded_tag_t SchemaNodeArray{0x75};
constexpr encoded_tag_t SchemaNodeObj{0x76};
}  // namespace Tag

constexpr encoded_tag_t EndOfStream{0x0};
}  // namespace clp_s::ffi::ir_stream::cProtocol

#endif  // FFI_IR_STREAM_PROTOCOL_CONSTANTS_HPP
