//! The result encoding of a streaming search, negotiated per request from its `Accept` header.

use axum::body::Bytes;
use axum::http::HeaderMap;
use axum::http::header::ACCEPT;
use clp_rust_utils::types::ArchiveId;
use search_result_listener::ArrowResultBatch;
use search_result_listener::ResultEncoder;
use search_result_listener::ResultEncoding;

use crate::arrow_ipc::ARROW_STREAM_CONTENT_TYPE;
use crate::arrow_ipc::ArrowIpcEncoder;
use crate::arrow_ipc::ArrowIpcEncoding;
use crate::sse::SseEncoder;
use crate::sse::SseEncoding;

/// The [`ResultEncoding`] a streaming search's response uses: Server-Sent Events, or an Arrow IPC
/// stream. Each batch holds the response's body chunks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NegotiatedEncoding {
    Sse,
    ArrowIpc,
}

impl NegotiatedEncoding {
    /// Negotiates the encoding of a response from the `Accept` header of its request.
    ///
    /// # Returns
    ///
    /// [`Self::ArrowIpc`] if the request accepts [`ARROW_STREAM_CONTENT_TYPE`] with a nonzero
    /// quality, or [`Self::Sse`] otherwise.
    #[must_use]
    pub fn from_headers(headers: &HeaderMap) -> Self {
        let accepts_arrow = headers
            .get_all(ACCEPT)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .any(|media_range| {
                let mut parameters = media_range.split(';').map(str::trim);
                parameters.next().is_some_and(|media_type| {
                    media_type.eq_ignore_ascii_case(ARROW_STREAM_CONTENT_TYPE)
                }) && !parameters.any(|parameter| {
                    parameter
                        .strip_prefix("q=")
                        .and_then(|quality| quality.parse::<f32>().ok())
                        .is_some_and(|quality| quality <= 0.0)
                })
            });
        if accepts_arrow {
            Self::ArrowIpc
        } else {
            Self::Sse
        }
    }
}

impl ResultEncoding for NegotiatedEncoding {
    type Batch = Vec<Bytes>;
    type Encoder = NegotiatedEncoder;

    fn encoder(&self, archive_id: ArchiveId) -> NegotiatedEncoder {
        match self {
            Self::Sse => NegotiatedEncoder::Sse(SseEncoding.encoder(archive_id)),
            Self::ArrowIpc => {
                NegotiatedEncoder::ArrowIpc(Box::new(ArrowIpcEncoding.encoder(archive_id)))
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
