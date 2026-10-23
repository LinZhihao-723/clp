//! Server-Sent Events encoding of streaming search results, and the response that streams the
//! encoded events.

use std::pin::Pin;
use std::task::Context;
use std::task::Poll;
use std::task::ready;
use std::time::Duration;

use axum::BoxError;
use axum::body::Body;
use axum::body::Bytes;
use axum::http::header::CACHE_CONTROL;
use axum::http::header::CONTENT_TYPE;
use axum::response::IntoResponse;
use axum::response::Response;
use clp_rust_utils::types::ArchiveId;
use futures::Stream;
use pin_project_lite::pin_project;
use search_result_listener::ResultEncoder;
use search_result_listener::ResultEncoding;
use serde::Serialize;
use tokio::time::Instant;
use tokio::time::Sleep;

/// The [`ResultEncoding`] of `POST /query/stream`: each result as an unnamed event whose data is
/// the result serialized as JSON, without its message's trailing newline.
#[derive(Clone, Copy, Debug, Default)]
pub struct SseEncoding;

impl ResultEncoding for SseEncoding {
    type Batch = Bytes;
    type Encoder = SseEncoder;

    /// # Panics
    ///
    /// Panics if serializing the archive ID into a `Vec` fails, which it never does.
    fn encoder(&self, archive_id: ArchiveId) -> SseEncoder {
        let mut event_prefix = b"data: {\"archive_id\":".to_vec();
        serde_json::to_writer(&mut event_prefix, &archive_id)
            .expect("serializing into a `Vec` shouldn't fail");
        event_prefix.extend_from_slice(b",\"timestamp\":");
        SseEncoder {
            event_prefix: event_prefix.into_boxed_slice(),
            batch: Vec::new(),
        }
    }
}

/// The [`ResultEncoder`] of [`SseEncoding`].
pub struct SseEncoder {
    event_prefix: Box<[u8]>,
    batch: Vec<u8>,
}

impl SseEncoder {
    /// Appends the event of one result to the pending batch.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`serde_json::to_writer`]'s return values on failure.
    fn write_event(&mut self, timestamp: i64, message: &str) -> Result<(), serde_json::Error> {
        self.batch.extend_from_slice(&self.event_prefix);
        serde_json::to_writer(&mut self.batch, &timestamp)?;
        self.batch.extend_from_slice(b",\"message\":");
        serde_json::to_writer(
            &mut self.batch,
            message.strip_suffix('\n').unwrap_or(message),
        )?;
        self.batch.extend_from_slice(b"}\n\n");
        Ok(())
    }
}

impl ResultEncoder for SseEncoder {
    type Batch = Bytes;

    /// # Panics
    ///
    /// Panics if serializing the result into a `Vec` fails, which it never does.
    fn encode(&mut self, timestamp: i64, message: &str) {
        self.write_event(timestamp, message)
            .expect("serializing into a `Vec` shouldn't fail");
    }

    fn take_batch(&mut self) -> Bytes {
        Bytes::from(std::mem::take(&mut self.batch))
    }
}

/// A Server-Sent Events response whose events are already encoded, laid out as
/// [`axum::response::Sse`] with [`axum::response::sse::KeepAlive::default`] lays them out.
///
/// # Type Parameters
///
/// * `EventStreamType` - The stream of the encoded events.
pub struct SseResponse<EventStreamType> {
    events: KeepAliveStream<EventStreamType>,
}

impl<EventStreamType> SseResponse<EventStreamType> {
    /// Factory function.
    ///
    /// Starts the keep-alive interval, as [`axum::response::Sse::keep_alive`] does.
    ///
    /// # Returns
    ///
    /// The newly created response.
    pub fn new(events: EventStreamType) -> Self {
        Self {
            events: KeepAliveStream::new(events, KEEP_ALIVE_INTERVAL),
        }
    }
}

impl<EventStreamType, ErrorType> IntoResponse for SseResponse<EventStreamType>
where
    EventStreamType: Stream<Item = Result<Bytes, ErrorType>> + Send + 'static,
    ErrorType: Into<BoxError>,
{
    fn into_response(self) -> Response {
        (
            [
                (CONTENT_TYPE, "text/event-stream"),
                (CACHE_CONTROL, "no-cache"),
            ],
            Body::from_stream(self.events),
        )
            .into_response()
    }
}

/// Encodes a named event, laid out as [`axum::response::sse::Event::json_data`] lays out an event
/// named with [`axum::response::sse::Event::event`].
///
/// # Type Parameters
///
/// * `DataType` - The type of the event's data.
///
/// # Returns
///
/// The event's bytes on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * Forwards [`serde_json::to_writer`]'s return values on failure.
pub fn encode_event<DataType: Serialize>(
    name: &str,
    data: &DataType,
) -> Result<Bytes, serde_json::Error> {
    let mut event = Vec::new();
    event.extend_from_slice(b"event: ");
    event.extend_from_slice(name.as_bytes());
    event.extend_from_slice(b"\ndata: ");
    serde_json::to_writer(&mut event, data)?;
    event.extend_from_slice(b"\n\n");
    Ok(Bytes::from(event))
}

const KEEP_ALIVE_COMMENT: &[u8] = b":\n\n";
const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(15);

pin_project! {
    /// A stream of encoded events that also yields a keep-alive comment whenever no event has been
    /// yielded for an interval.
    ///
    /// # Type Parameters
    ///
    /// * `EventStreamType` - The stream of the encoded events.
    struct KeepAliveStream<EventStreamType> {
        #[pin]
        events: EventStreamType,
        #[pin]
        timer: Sleep,
        interval: Duration,
    }
}

impl<EventStreamType> KeepAliveStream<EventStreamType> {
    /// Factory function.
    ///
    /// Starts the keep-alive interval.
    ///
    /// # Returns
    ///
    /// The newly created stream.
    fn new(events: EventStreamType, interval: Duration) -> Self {
        Self {
            events,
            timer: tokio::time::sleep(interval),
            interval,
        }
    }
}

impl<EventStreamType: Stream<Item = Result<Bytes, ErrorType>>, ErrorType> Stream
    for KeepAliveStream<EventStreamType>
{
    type Item = Result<Bytes, ErrorType>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();
        match this.events.poll_next(cx) {
            Poll::Ready(Some(Ok(event))) => {
                this.timer.reset(Instant::now() + *this.interval);
                Poll::Ready(Some(Ok(event)))
            }
            Poll::Ready(end_or_error) => Poll::Ready(end_or_error),
            Poll::Pending => {
                ready!(this.timer.as_mut().poll(cx));
                this.timer.reset(Instant::now() + *this.interval);
                Poll::Ready(Some(Ok(Bytes::from_static(KEEP_ALIVE_COMMENT))))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::convert::Infallible;
    use std::pin::pin;
    use std::time::Duration;

    use axum::body::Bytes;
    use axum::response::IntoResponse;
    use axum::response::Sse;
    use axum::response::sse::Event;
    use axum::response::sse::KeepAlive;
    use futures::StreamExt;
    use futures::stream;
    use http_body_util::BodyExt;
    use tokio::time::Instant;
    use tokio::time::sleep;
    use tokio::time::timeout;

    use super::KeepAliveStream;

    const INTERVAL: Duration = Duration::from_millis(50);

    /// The longest a test waits for a stream before failing.
    const TIMEOUT: Duration = Duration::from_secs(10);

    #[tokio::test]
    async fn idle_response_sends_keep_alive_comments() -> anyhow::Result<()> {
        let mut axum_body = Sse::new(stream::pending::<Result<Event, Infallible>>())
            .keep_alive(KeepAlive::new().interval(INTERVAL))
            .into_response()
            .into_body();
        let axum_comment = timeout(TIMEOUT, axum_body.frame())
            .await?
            .expect("the axum body shouldn't end")?
            .into_data()
            .expect("the axum frame should hold data");
        let mut keep_alive = pin!(KeepAliveStream::new(
            stream::pending::<Result<Bytes, Infallible>>(),
            INTERVAL
        ));

        for _ in 0..2 {
            assert_eq!(
                timeout(TIMEOUT, keep_alive.next()).await?,
                Some(Ok(axum_comment.clone()))
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn event_postpones_the_next_keep_alive_comment_by_the_interval() -> anyhow::Result<()> {
        const EVENT: &[u8] = b"data: 1\n\n";

        let events = stream::once(async {
            sleep(INTERVAL * 3 / 5).await;
            Ok::<_, Infallible>(Bytes::from_static(EVENT))
        })
        .chain(stream::pending());
        let mut keep_alive = pin!(KeepAliveStream::new(events, INTERVAL));

        assert_eq!(
            timeout(TIMEOUT, keep_alive.next()).await?,
            Some(Ok(Bytes::from_static(EVENT)))
        );
        let event_received_at = Instant::now();
        assert_eq!(
            timeout(TIMEOUT, keep_alive.next()).await?,
            Some(Ok(Bytes::from_static(b":\n\n")))
        );
        let comment_delay = event_received_at.elapsed();

        assert!(
            comment_delay >= INTERVAL * 4 / 5,
            "the keep-alive comment came {comment_delay:?} after the event"
        );
        Ok(())
    }
}
