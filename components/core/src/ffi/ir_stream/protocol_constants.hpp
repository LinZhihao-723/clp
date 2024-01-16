#ifndef FFI_IR_STREAM_PROTOCOL_CONSTANTS_HPP
#define FFI_IR_STREAM_PROTOCOL_CONSTANTS_HPP

#include <cstddef>
#include <cstdint>
#include <type_traits>

namespace ffi::ir_stream::cProtocol {
namespace Metadata {
    constexpr int8_t EncodingJson = 0x1;
    constexpr int8_t LengthUByte = 0x11;
    constexpr int8_t LengthUShort = 0x12;

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

namespace Payload {
    constexpr int8_t VarFourByteEncoding = 0x18;
    constexpr int8_t VarEightByteEncoding = 0x19;

    constexpr int8_t VarStrLenUByte = 0x11;
    constexpr int8_t VarStrLenUShort = 0x12;
    constexpr int8_t VarStrLenInt = 0x13;

    constexpr int8_t LogtypeStrLenUByte = 0x21;
    constexpr int8_t LogtypeStrLenUShort = 0x22;
    constexpr int8_t LogtypeStrLenInt = 0x23;

    constexpr int8_t TimestampVal = 0x30;
    constexpr int8_t TimestampDeltaByte = 0x31;
    constexpr int8_t TimestampDeltaShort = 0x32;
    constexpr int8_t TimestampDeltaInt = 0x33;
    constexpr int8_t TimestampDeltaLong = 0x34;

    constexpr int8_t ValueInt8 = 0x50;
    constexpr int8_t ValueInt16 = 0x51;
    constexpr int8_t ValueInt32 = 0x52;
    constexpr int8_t ValueInt64 = 0x53;
    constexpr int8_t ValueTrue = 0x54;
    constexpr int8_t ValueFalse = 0x55;
    constexpr int8_t ValueDouble = 0x56;

    constexpr int8_t ValueStrCLP = 0x57;
    constexpr int8_t ValueStrLenUByte = 0x58;
    constexpr int8_t ValueStrLenUShort = 0x59;
    constexpr int8_t ValueStrLenUInt = 0x5a;

    constexpr int8_t ValueNull = 0x5f;

    constexpr int8_t SchemaNodeIdByte = 0x60;
    constexpr int8_t SchemaNodeIdShort = 0x61;
    constexpr int8_t ArrayBegin = 0x62;
    constexpr int8_t ArrayEnd = 0x63;
    constexpr int8_t KeyValuePairRecordDeliminator = 0x64;
    constexpr int8_t SchemaNodeParentIdByte = 0x65;
    constexpr int8_t SchemaNodeParentIdShort = 0x66;
    constexpr int8_t SchemaNodeNameLenByte = 0x69;
    constexpr int8_t SchemaNodeNameLenShort = 0x6a;
    constexpr int8_t EmptyArray = 0x6e;
    constexpr int8_t EmptyObj = 0x6f;

    constexpr int8_t SchemaNodeUnknown = 0x70;
    constexpr int8_t SchemaNodeInt = 0x71;
    constexpr int8_t SchemaNodeFloat = 0x72;
    constexpr int8_t SchemaNodeBool = 0x73;
    constexpr int8_t SchemaNodeStr = 0x74;
    constexpr int8_t SchemaNodeObj = 0x75;
}  // namespace Payload

constexpr int8_t FourByteEncodingMagicNumber[]
        = {static_cast<int8_t>(0xFD), 0x2F, static_cast<int8_t>(0xB5), 0x29};
constexpr int8_t EightByteEncodingMagicNumber[]
        = {static_cast<int8_t>(0xFD), 0x2F, static_cast<int8_t>(0xB5), 0x30};
constexpr std::enable_if<
        sizeof(EightByteEncodingMagicNumber) == sizeof(FourByteEncodingMagicNumber),
        size_t>::type MagicNumberLength
        = sizeof(EightByteEncodingMagicNumber);
constexpr int8_t Eof = 0x0;
}  // namespace ffi::ir_stream::cProtocol

#endif  // FFI_IR_STREAM_PROTOCOL_CONSTANTS_HPP
