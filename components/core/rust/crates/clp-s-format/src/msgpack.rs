//! A minimal, allocation-free msgpack reader for the metadata packets.

use crate::CorruptionReason;
use crate::FormatError;
use crate::bytes_reader::ByteReader;

/// One msgpack item; containers carry only their length, and their elements follow in the
/// stream.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Item<'bytes> {
    Nil,
    Boolean(bool),
    UnsignedInteger(u64),
    Integer(i64),
    Float(f64),
    String(&'bytes [u8]),
    Binary(&'bytes [u8]),
    Extension(&'bytes [u8]),
    Array(u32),
    Map(u32),
}

/// Reads the next item header (a whole scalar, or the length of a container).
///
/// # Returns
///
/// The item on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`FormatError::Corrupt`] if the marker is reserved.
/// * Forwards [`ByteReader::read_u8`]'s return values on failure.
/// * Forwards [`ByteReader::read_array`]'s return values on failure.
/// * Forwards [`ByteReader::read_bytes`]'s return values on failure.
pub fn read_item<'bytes>(reader: &mut ByteReader<'bytes>) -> Result<Item<'bytes>, FormatError> {
    let marker = reader.read_u8()?;
    let item = match marker {
        0x00..=0x7f => Item::UnsignedInteger(u64::from(marker)),
        0x80..=0x8f => Item::Map(u32::from(marker & 0x0f)),
        0x90..=0x9f => Item::Array(u32::from(marker & 0x0f)),
        0xa0..=0xbf => Item::String(reader.read_bytes(u64::from(marker & 0x1f))?),
        0xc0 => Item::Nil,
        0xc2 => Item::Boolean(false),
        0xc3 => Item::Boolean(true),
        0xc4 => {
            let size = read_length::<1>(reader)?;
            Item::Binary(reader.read_bytes(size)?)
        }
        0xc5 => {
            let size = read_length::<2>(reader)?;
            Item::Binary(reader.read_bytes(size)?)
        }
        0xc6 => {
            let size = read_length::<4>(reader)?;
            Item::Binary(reader.read_bytes(size)?)
        }
        0xc7 => {
            let size = read_length::<1>(reader)?;
            read_extension(reader, size)?
        }
        0xc8 => {
            let size = read_length::<2>(reader)?;
            read_extension(reader, size)?
        }
        0xc9 => {
            let size = read_length::<4>(reader)?;
            read_extension(reader, size)?
        }
        0xca => Item::Float(f64::from(f32::from_be_bytes(reader.read_array()?))),
        0xcb => Item::Float(f64::from_be_bytes(reader.read_array()?)),
        0xcc => Item::UnsignedInteger(read_length::<1>(reader)?),
        0xcd => Item::UnsignedInteger(read_length::<2>(reader)?),
        0xce => Item::UnsignedInteger(read_length::<4>(reader)?),
        0xcf => Item::UnsignedInteger(u64::from_be_bytes(reader.read_array()?)),
        0xd0 => Item::Integer(i64::from(i8::from_be_bytes(reader.read_array()?))),
        0xd1 => Item::Integer(i64::from(i16::from_be_bytes(reader.read_array()?))),
        0xd2 => Item::Integer(i64::from(i32::from_be_bytes(reader.read_array()?))),
        0xd3 => Item::Integer(i64::from_be_bytes(reader.read_array()?)),
        0xd4 => read_extension(reader, 1)?,
        0xd5 => read_extension(reader, 2)?,
        0xd6 => read_extension(reader, 4)?,
        0xd7 => read_extension(reader, 8)?,
        0xd8 => read_extension(reader, 16)?,
        0xd9 => {
            let size = read_length::<1>(reader)?;
            Item::String(reader.read_bytes(size)?)
        }
        0xda => {
            let size = read_length::<2>(reader)?;
            Item::String(reader.read_bytes(size)?)
        }
        0xdb => {
            let size = read_length::<4>(reader)?;
            Item::String(reader.read_bytes(size)?)
        }
        0xdc => Item::Array(read_container_length::<2>(reader)?),
        0xdd => Item::Array(read_container_length::<4>(reader)?),
        0xde => Item::Map(read_container_length::<2>(reader)?),
        0xdf => Item::Map(read_container_length::<4>(reader)?),
        0xe0..=0xff => Item::Integer(i64::from(i8::from_be_bytes([marker]))),
        0xc1 => return Err(reader.corrupt(CorruptionReason::MalformedMsgpack)),
    };
    Ok(item)
}

/// Skips one complete value, including every element of nested containers, without recursion.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`FormatError::Corrupt`] if the containers claim more elements than bytes remain.
/// * Forwards [`read_item`]'s return values on failure.
pub fn skip_value(reader: &mut ByteReader<'_>) -> Result<(), FormatError> {
    let mut num_pending_items: u64 = 1;
    while num_pending_items > 0 {
        num_pending_items -= 1;
        let num_children = match read_item(reader)? {
            Item::Array(len) => u64::from(len),
            Item::Map(len) => 2 * u64::from(len),
            _ => 0,
        };
        num_pending_items += num_children;
        // Every item takes at least one byte, which bounds the pending count by the input size.
        if num_pending_items > reader.remaining_size() as u64 {
            return Err(reader.corrupt(CorruptionReason::MalformedMsgpack));
        }
    }
    Ok(())
}

/// Skips the elements of a container whose header `item` was just read; does nothing for
/// scalars.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`FormatError::Corrupt`] if the container claims more elements than bytes remain.
/// * Forwards [`skip_value`]'s return values on failure.
pub fn skip_children(reader: &mut ByteReader<'_>, item: Item<'_>) -> Result<(), FormatError> {
    let num_children = match item {
        Item::Array(len) => u64::from(len),
        Item::Map(len) => 2 * u64::from(len),
        _ => return Ok(()),
    };
    reader.check_count(num_children, 1)?;
    for _ in 0..num_children {
        skip_value(reader)?;
    }
    Ok(())
}

/// # Returns
///
/// The big-endian unsigned length of `SIZE` bytes on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`ByteReader::read_array`]'s return values on failure.
fn read_length<const SIZE: usize>(reader: &mut ByteReader<'_>) -> Result<u64, FormatError> {
    let bytes = reader.read_array::<SIZE>()?;
    Ok(bytes
        .iter()
        .fold(0_u64, |value, byte| (value << 8) | u64::from(*byte)))
}

/// # Returns
///
/// The big-endian container length of `SIZE` bytes on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`FormatError::Corrupt`] if the length does not fit in `u32`.
/// * Forwards [`read_length`]'s return values on failure.
fn read_container_length<const SIZE: usize>(
    reader: &mut ByteReader<'_>,
) -> Result<u32, FormatError> {
    u32::try_from(read_length::<SIZE>(reader)?)
        .map_err(|_| reader.corrupt(CorruptionReason::MalformedMsgpack))
}

/// Reads an extension's type byte and its `size` data bytes.
///
/// # Returns
///
/// The extension data on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`ByteReader::read_u8`]'s return values on failure.
/// * Forwards [`ByteReader::read_bytes`]'s return values on failure.
fn read_extension<'bytes>(
    reader: &mut ByteReader<'bytes>,
    size: u64,
) -> Result<Item<'bytes>, FormatError> {
    reader.read_u8()?;
    Ok(Item::Extension(reader.read_bytes(size)?))
}

#[cfg(test)]
mod tests {
    use super::Item;
    use super::read_item;
    use super::skip_children;
    use super::skip_value;
    use crate::FormatLocation;
    use crate::bytes_reader::ByteReader;

    #[test]
    fn reads_every_scalar_encoding() -> anyhow::Result<()> {
        let encoded: &[u8] = &[
            0x05, // positive fixint
            0xff, // negative fixint -1
            0xc0, // nil
            0xc3, // true
            0xcc, 0xc8, // u8 200
            0xcd, 0x01, 0x00, // u16 256
            0xcf, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, // u64 max
            0xd0, 0x80, // i8 -128
            0xd3, 0x80, 0, 0, 0, 0, 0, 0, 0, // i64 min
            0xcb, 0x3f, 0xf8, 0, 0, 0, 0, 0, 0, // f64 1.5
            0xca, 0x3f, 0xc0, 0, 0, // f32 1.5
            0xa2, b'h', b'i', // fixstr
            0xd9, 0x01, b'x', // str8
            0xc4, 0x02, 1, 2, // bin8
            0xd4, 0x07, 0x09, // fixext1
        ];
        let expected = [
            Item::UnsignedInteger(5),
            Item::Integer(-1),
            Item::Nil,
            Item::Boolean(true),
            Item::UnsignedInteger(200),
            Item::UnsignedInteger(256),
            Item::UnsignedInteger(u64::MAX),
            Item::Integer(-128),
            Item::Integer(i64::MIN),
            Item::Float(1.5),
            Item::Float(1.5),
            Item::String(b"hi"),
            Item::String(b"x"),
            Item::Binary(&[1, 2]),
            Item::Extension(&[0x09]),
        ];
        let mut reader = ByteReader::new(encoded, FormatLocation::Header);
        for expected_item in expected {
            assert_eq!(read_item(&mut reader)?, expected_item);
        }
        assert_eq!(reader.remaining_size(), 0);
        Ok(())
    }

    #[test]
    fn skips_nested_containers() -> anyhow::Result<()> {
        // {"a": [1, {"b": nil}], "c": 2}, then a trailing byte.
        let encoded: &[u8] = &[
            0x82, 0xa1, b'a', 0x92, 0x01, 0x81, 0xa1, b'b', 0xc0, 0xa1, b'c', 0x02, 0x07,
        ];
        let mut reader = ByteReader::new(encoded, FormatLocation::Header);
        skip_value(&mut reader)?;
        assert_eq!(reader.remaining_size(), 1);

        let mut reader = ByteReader::new(encoded, FormatLocation::Header);
        let header = read_item(&mut reader)?;
        assert_eq!(header, Item::Map(2));
        skip_children(&mut reader, header)?;
        assert_eq!(reader.remaining_size(), 1);
        Ok(())
    }

    #[test]
    fn rejects_reserved_markers_and_oversized_containers() {
        for encoded in [
            &[0xc1_u8][..],
            &[0xdd, 0xff, 0xff, 0xff, 0xff, 0x01][..],
            &[0x92, 0x01][..],
            &[0xdb, 0x00, 0x00, 0x00, 0x10, b'a'][..],
        ] {
            let mut reader = ByteReader::new(encoded, FormatLocation::Header);
            assert!(
                skip_value(&mut reader).is_err(),
                "{encoded:02x?} must be rejected"
            );
        }
    }

    #[test]
    fn deeply_nested_arrays_do_not_recurse() -> anyhow::Result<()> {
        let mut encoded = vec![0x91_u8; 100_000];
        encoded.push(0xc0);
        let mut reader = ByteReader::new(&encoded, FormatLocation::Header);
        skip_value(&mut reader)?;
        assert_eq!(reader.remaining_size(), 0);
        Ok(())
    }
}
