//! The result encoding of a streaming search, negotiated per request from its `Accept` header.

use axum::body::Bytes;
use axum::http::HeaderMap;
use axum::http::header::ACCEPT;
use clp_rust_utils::types::ArchiveId;
use search_result_listener::ArrowResultBatch;
use search_result_listener::ResultEncoder;
use search_result_listener::ResultEncoding;

use crate::arrow_ipc::ARROW_STREAM_CONTENT_TYPE;
use crate::arrow_ipc::ARROW_STREAM_ZSTD_CONTENT_TYPE;
use crate::arrow_ipc::ArrowIpcEncoder;
use crate::arrow_ipc::ArrowIpcEncoding;
use crate::sse::SseEncoder;
use crate::sse::SseEncoding;

/// The [`ResultEncoding`] a streaming search's response uses.
///
/// The response is Server-Sent Events, an Arrow IPC stream, or an Arrow IPC stream whose record
/// batches are compressed with zstd. Each batch holds the response's body chunks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NegotiatedEncoding {
    Sse,
    ArrowIpc,
    ArrowIpcZstd,
}

impl NegotiatedEncoding {
    /// Negotiates the encoding of a response from the `Accept` header of its request.
    ///
    /// # Returns
    ///
    /// The Arrow IPC encoding accepted with the highest nonzero quality, preferring
    /// [`Self::ArrowIpcZstd`] on a tie, if that quality is at least the quality of
    /// `text/event-stream`, or [`Self::Sse`] otherwise.
    #[must_use]
    pub fn from_headers(headers: &HeaderMap) -> Self {
        let media_ranges: Vec<MediaRange<'_>> = headers
            .get_all(ACCEPT)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .filter_map(MediaRange::parse)
            .collect();
        let quality_of = |most_specific_first: &[&str]| {
            most_specific_first
                .iter()
                .find_map(|media_type| {
                    media_ranges
                        .iter()
                        .filter(|media_range| {
                            media_range.media_type.eq_ignore_ascii_case(media_type)
                        })
                        .map(|media_range| media_range.quality)
                        .reduce(f32::max)
                })
                .unwrap_or(0.0)
        };
        let arrow_quality_of = |compression: Option<&str>| {
            media_ranges
                .iter()
                .filter(|media_range| {
                    media_range
                        .media_type
                        .eq_ignore_ascii_case(ARROW_STREAM_CONTENT_TYPE)
                        && match (media_range.compression, compression) {
                            (None, None) => true,
                            (Some(requested), Some(compression)) => {
                                requested.eq_ignore_ascii_case(compression)
                            }
                            _ => false,
                        }
                })
                .map(|media_range| media_range.quality)
                .reduce(f32::max)
                .unwrap_or(0.0)
        };
        let arrow_quality = arrow_quality_of(None);
        let arrow_zstd_quality = arrow_quality_of(Some(ZSTD_COMPRESSION));
        let sse_quality = quality_of(&[SSE_CONTENT_TYPE, "text/*", "*/*"]);
        if arrow_zstd_quality > 0.0
            && arrow_zstd_quality >= arrow_quality
            && arrow_zstd_quality >= sse_quality
        {
            Self::ArrowIpcZstd
        } else if arrow_quality > 0.0 && arrow_quality >= sse_quality {
            Self::ArrowIpc
        } else {
            Self::Sse
        }
    }

    /// # Returns
    ///
    /// The media type of the response, which an Arrow IPC encoding carries in its
    /// `Content-Type` header, or `None` for [`Self::Sse`], whose response sets its own.
    #[must_use]
    pub const fn arrow_content_type(self) -> Option<&'static str> {
        match self {
            Self::Sse => None,
            Self::ArrowIpc => Some(ARROW_STREAM_CONTENT_TYPE),
            Self::ArrowIpcZstd => Some(ARROW_STREAM_ZSTD_CONTENT_TYPE),
        }
    }
}

impl ResultEncoding for NegotiatedEncoding {
    type Batch = Vec<Bytes>;
    type Encoder = NegotiatedEncoder;

    fn encoder(&self, archive_id: ArchiveId) -> NegotiatedEncoder {
        match self {
            Self::Sse => NegotiatedEncoder::Sse(SseEncoding.encoder(archive_id)),
            Self::ArrowIpc => NegotiatedEncoder::ArrowIpc(Box::new(
                ArrowIpcEncoding::uncompressed().encoder(archive_id),
            )),
            Self::ArrowIpcZstd => {
                NegotiatedEncoder::ArrowIpc(Box::new(ArrowIpcEncoding::zstd().encoder(archive_id)))
            }
        }
    }
}

/// The [`ResultEncoder`] of [`NegotiatedEncoding`].
pub enum NegotiatedEncoder {
    Sse(SseEncoder),
    ArrowIpc(Box<ArrowIpcEncoder>),
}

impl ResultEncoder for NegotiatedEncoder {
    type Batch = Vec<Bytes>;

    fn encode(&mut self, timestamp: i64, message: &str) {
        match self {
            Self::Sse(encoder) => encoder.encode(timestamp, message),
            Self::ArrowIpc(encoder) => encoder.encode(timestamp, message),
        }
    }

    fn encode_arrow(&mut self, batch: &ArrowResultBatch, num_duplicates: usize) {
        match self {
            Self::Sse(encoder) => encoder.encode_arrow(batch, num_duplicates),
            Self::ArrowIpc(encoder) => encoder.encode_arrow(batch, num_duplicates),
        }
    }

    fn take_batch(&mut self) -> Vec<Bytes> {
        match self {
            Self::Sse(encoder) => vec![encoder.take_batch()],
            Self::ArrowIpc(encoder) => encoder.take_batch(),
        }
    }
}

const SSE_CONTENT_TYPE: &str = "text/event-stream";
const ZSTD_COMPRESSION: &str = "zstd";
const DEFAULT_QUALITY: f32 = 1.0;

/// One media range of an `Accept` header.
struct MediaRange<'header_lifetime> {
    media_type: &'header_lifetime str,
    quality: f32,

    /// The value of the media range's `compression` parameter, unquoted.
    compression: Option<&'header_lifetime str>,
}

impl<'header_lifetime> MediaRange<'header_lifetime> {
    /// Parses a media range, ignoring a quality parameter that isn't a number.
    ///
    /// # Returns
    ///
    /// The parsed media range, or `None` if `media_range` names no media type.
    fn parse(media_range: &'header_lifetime str) -> Option<Self> {
        let mut parameters = media_range.split(';').map(str::trim);
        let media_type = parameters
            .next()
            .filter(|media_type| !media_type.is_empty())?;
        let mut quality = DEFAULT_QUALITY;
        let mut compression = None;
        for (name, value) in parameters.filter_map(|parameter| parameter.split_once('=')) {
            let name = name.trim();
            let value = value.trim();
            if name.eq_ignore_ascii_case("q") {
                if let Ok(parsed_quality) = value.parse::<f32>() {
                    quality = parsed_quality;
                }
            } else if name.eq_ignore_ascii_case("compression") {
                compression = Some(
                    value
                        .strip_prefix('"')
                        .and_then(|value| value.strip_suffix('"'))
                        .unwrap_or(value),
                );
            }
        }
        Some(Self {
            media_type,
            quality,
            compression,
        })
    }
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderMap;
    use axum::http::HeaderValue;
    use axum::http::header::ACCEPT;

    use super::NegotiatedEncoding;

    #[test]
    fn arrow_is_negotiated_only_when_accepted() {
        for (accept_values, expected) in [
            (&[][..], NegotiatedEncoding::Sse),
            (&["text/event-stream"][..], NegotiatedEncoding::Sse),
            (&["*/*"][..], NegotiatedEncoding::Sse),
            (
                &["application/vnd.apache.arrow.stream"][..],
                NegotiatedEncoding::ArrowIpc,
            ),
            (
                &["text/event-stream;q=0.5, Application/Vnd.Apache.Arrow.Stream"][..],
                NegotiatedEncoding::ArrowIpc,
            ),
            (
                &["text/html", "application/vnd.apache.arrow.stream; q=0.9"][..],
                NegotiatedEncoding::ArrowIpc,
            ),
            (
                &["application/vnd.apache.arrow.stream;q=0"][..],
                NegotiatedEncoding::Sse,
            ),
            (
                &["application/vnd.apache.arrow.file"][..],
                NegotiatedEncoding::Sse,
            ),
            (
                &["application/vnd.apache.arrow.stream;Q=0"][..],
                NegotiatedEncoding::Sse,
            ),
            (
                &["application/vnd.apache.arrow.stream;q=0.1, text/event-stream"][..],
                NegotiatedEncoding::Sse,
            ),
            (
                &["application/vnd.apache.arrow.stream;q=0.5, */*"][..],
                NegotiatedEncoding::Sse,
            ),
            (
                &["application/vnd.apache.arrow.stream;q=0.5, text/*;q=0.5"][..],
                NegotiatedEncoding::ArrowIpc,
            ),
            (
                &["application/vnd.apache.arrow.stream;q=0.5, text/*;q=0.9, \
                   text/event-stream;q=0.2"][..],
                NegotiatedEncoding::ArrowIpc,
            ),
            (
                &["application/vnd.apache.arrow.stream; compression=zstd"][..],
                NegotiatedEncoding::ArrowIpcZstd,
            ),
            (
                &["Application/Vnd.Apache.Arrow.Stream;Compression=\"ZSTD\";q=0.8"][..],
                NegotiatedEncoding::ArrowIpcZstd,
            ),
            (
                &["application/vnd.apache.arrow.stream; compression=lz4"][..],
                NegotiatedEncoding::Sse,
            ),
            (
                &["application/vnd.apache.arrow.stream; compression=lz4, \
                   application/vnd.apache.arrow.stream; q=0.1"][..],
                NegotiatedEncoding::ArrowIpc,
            ),
            (
                &[
                    "application/vnd.apache.arrow.stream, application/vnd.apache.arrow.stream; \
                     compression=zstd",
                ][..],
                NegotiatedEncoding::ArrowIpcZstd,
            ),
            (
                &[
                    "application/vnd.apache.arrow.stream; compression=zstd; q=0.5, \
                     application/vnd.apache.arrow.stream",
                ][..],
                NegotiatedEncoding::ArrowIpc,
            ),
            (
                &[
                    "application/vnd.apache.arrow.stream; compression=zstd; q=0, \
                     application/vnd.apache.arrow.stream",
                ][..],
                NegotiatedEncoding::ArrowIpc,
            ),
            (
                &["application/vnd.apache.arrow.stream; compression=zstd; q=0"][..],
                NegotiatedEncoding::Sse,
            ),
            (
                &[
                    "application/vnd.apache.arrow.stream; compression=zstd; q=0.5",
                    "*/*",
                ][..],
                NegotiatedEncoding::Sse,
            ),
            (
                &[
                    "application/vnd.apache.arrow.stream; compression=zstd; q=0.5, \
                     text/event-stream; q=0.5",
                ][..],
                NegotiatedEncoding::ArrowIpcZstd,
            ),
        ] {
            let mut headers = HeaderMap::new();
            for value in accept_values {
                headers.append(ACCEPT, HeaderValue::from_static(value));
            }
            assert_eq!(
                NegotiatedEncoding::from_headers(&headers),
                expected,
                "{accept_values:?}"
            );
        }
    }
}
