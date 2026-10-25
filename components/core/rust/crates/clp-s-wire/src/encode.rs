//! Msgpack encoding helpers shared by the wire formats.

use crate::WireError;

/// Appends a record's message to `output` as the C++ outputs write it: the text followed by `\n`,
/// with invalid UTF-8 replaced by U+FFFD.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`WireError::Msgpack`] if the message is longer than msgpack's `u32` length limit.
/// * Forwards [`write_str_header`]'s return values on failure.
pub fn write_message(output: &mut Vec<u8>, field: &str, message: &[u8]) -> Result<(), WireError> {
    let message = String::from_utf8_lossy(message);
    let message_size = message
        .len()
        .checked_add(1)
        .ok_or_else(|| too_long(field, message.len()))?;
    write_str_header(output, field, message_size)?;
    output.extend_from_slice(message.as_bytes());
    output.push(b'\n');
    Ok(())
}

/// Appends the msgpack string `text` to `output`.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`write_str_header`]'s return values on failure.
pub fn write_str(output: &mut Vec<u8>, field: &str, text: &str) -> Result<(), WireError> {
    write_str_header(output, field, text.len())?;
    output.extend_from_slice(text.as_bytes());
    Ok(())
}

/// Appends the header of a msgpack string of `size` bytes to `output`.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`WireError::Msgpack`] if `size` exceeds msgpack's `u32` length limit.
pub fn write_str_header(output: &mut Vec<u8>, field: &str, size: usize) -> Result<(), WireError> {
    let length = u32::try_from(size).map_err(|_| too_long(field, size))?;
    rmp::encode::write_str_len(output, length).map_err(msgpack_error)?;
    Ok(())
}

/// # Type Parameters
///
/// * `ErrorType` - The error of the failed msgpack write.
///
/// # Returns
///
/// The wire error of a failed msgpack write.
pub fn msgpack_error<ErrorType: std::fmt::Display>(error: ErrorType) -> WireError {
    WireError::Msgpack(error.to_string().into())
}

/// # Returns
///
/// The error of a `field` string of `size` bytes that msgpack cannot hold.
fn too_long(field: &str, size: usize) -> WireError {
    WireError::Msgpack(
        format!("the {field} of {size} bytes exceeds the msgpack string limit").into(),
    )
}
