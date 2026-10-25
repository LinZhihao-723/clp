//! The channel that carries a session's batches to its consumer, bounded by the bytes of the
//! batches it buffers rather than by their number.

use std::num::NonZeroU32;
use std::sync::Arc;
use std::task::Context;
use std::task::Poll;

use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;

use crate::encoding::EncodedBatch;

/// Creates a channel that buffers batches of up to `size` bytes in total. A batch larger than
/// `size` is buffered alone.
///
/// # Type Parameters
///
/// * `BatchType` - The type of the batches.
///
/// # Returns
///
/// The sender and the receiver of the newly created channel.
pub fn channel<BatchType: EncodedBatch>(
    size: NonZeroU32,
) -> (Sender<BatchType>, Receiver<BatchType>) {
    let (batches_sender, batches_receiver) = mpsc::unbounded_channel();
    let budget = Arc::new(Semaphore::new(size.get() as usize));
    (
        Sender {
            batches: batches_sender,
            budget: Arc::clone(&budget),
            size,
        },
        Receiver {
            batches: batches_receiver,
            budget,
        },
    )
}

/// The sending half of a [`channel`].
///
/// # Type Parameters
///
/// * `BatchType` - The type of the batches.
pub struct Sender<BatchType: EncodedBatch> {
    batches: mpsc::UnboundedSender<(BatchType, OwnedSemaphorePermit)>,
    budget: Arc<Semaphore>,
    size: NonZeroU32,
}

impl<BatchType: EncodedBatch> Sender<BatchType> {
    /// Sends `batch`, waiting until the channel has room for it.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * `batch` itself if the receiver has been dropped.
    pub async fn send(&self, batch: BatchType) -> Result<(), BatchType> {
        let size = self.size.get();
        let num_permits =
            u32::try_from(batch.size()).map_or(size, |batch_size| batch_size.min(size));
        let Ok(permit) = Arc::clone(&self.budget)
            .acquire_many_owned(num_permits)
            .await
        else {
            return Err(batch);
        };
        self.batches
            .send((batch, permit))
            .map_err(|error| error.0.0)
    }
}

/// The receiving half of a [`channel`]. Dropping it discards the buffered batches, and makes every
/// pending and later send fail.
///
/// # Type Parameters
///
/// * `BatchType` - The type of the batches.
pub struct Receiver<BatchType: EncodedBatch> {
    batches: mpsc::UnboundedReceiver<(BatchType, OwnedSemaphorePermit)>,
    budget: Arc<Semaphore>,
}

impl<BatchType: EncodedBatch> Receiver<BatchType> {
    /// Polls for the next batch, which frees its room in the channel once received.
    ///
    /// # Returns
    ///
    /// * [`Poll::Ready`] with the next batch, or `None` once every sender has been dropped and the
    ///   channel is empty.
    /// * [`Poll::Pending`] if no batch is buffered yet.
    pub fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<Option<BatchType>> {
        self.batches
            .poll_recv(cx)
            .map(|received| received.map(|(batch, _permit)| batch))
    }
}

impl<BatchType: EncodedBatch> Drop for Receiver<BatchType> {
    fn drop(&mut self) {
        self.budget.close();
    }
}

#[cfg(test)]
mod tests {
    use std::future::poll_fn;
    use std::num::NonZeroU32;
    use std::time::Duration;

    use bytes::Bytes;
    use tokio::time::timeout;

    use super::channel;

    const WAIT: Duration = Duration::from_millis(100);

    #[tokio::test]
    async fn sends_wait_for_room_in_bytes() {
        let (sender, mut receiver) = channel::<Bytes>(NonZeroU32::new(10).expect("10 is nonzero"));
        sender
            .send(Bytes::from_static(b"0123456"))
            .await
            .expect("7 bytes should fit in 10");
        assert!(
            timeout(WAIT, sender.send(Bytes::from_static(b"abcd")))
                .await
                .is_err(),
            "11 bytes shouldn't fit in 10"
        );

        assert_eq!(
            poll_fn(|cx| receiver.poll_recv(cx)).await,
            Some(Bytes::from_static(b"0123456"))
        );
        timeout(WAIT, sender.send(Bytes::from_static(b"abcd")))
            .await
            .expect("4 bytes should fit once the first batch is received")
            .expect("the receiver is alive");
    }

    #[tokio::test]
    async fn oversized_batches_are_buffered_alone() {
        let (sender, mut receiver) = channel::<Bytes>(NonZeroU32::new(4).expect("4 is nonzero"));
        timeout(WAIT, sender.send(Bytes::from_static(b"0123456789")))
            .await
            .expect("an oversized batch should fit in an empty channel")
            .expect("the receiver is alive");
        assert!(
            timeout(WAIT, sender.send(Bytes::from_static(b"a")))
                .await
                .is_err(),
            "nothing should fit next to an oversized batch"
        );
        assert_eq!(
            poll_fn(|cx| receiver.poll_recv(cx)).await,
            Some(Bytes::from_static(b"0123456789"))
        );
    }

    #[tokio::test]
    async fn dropping_the_receiver_fails_pending_and_later_sends() {
        let (sender, receiver) = channel::<Bytes>(NonZeroU32::new(4).expect("4 is nonzero"));
        sender
            .send(Bytes::from_static(b"0123"))
            .await
            .expect("4 bytes should fit in 4");
        let pending = tokio::spawn(async move {
            let result = sender.send(Bytes::from_static(b"a")).await;
            (sender, result)
        });
        tokio::time::sleep(WAIT).await;

        drop(receiver);
        let (sender, result) = timeout(WAIT, pending)
            .await
            .expect("the pending send should end once the receiver is dropped")
            .expect("the sending task shouldn't panic");
        assert_eq!(result, Err(Bytes::from_static(b"a")));
        assert_eq!(
            sender.send(Bytes::from_static(b"b")).await,
            Err(Bytes::from_static(b"b"))
        );
    }
}
