//! The TCP listener that accepts connections from search tasks and routes them to sessions.

use std::io::ErrorKind;
use std::net::IpAddr;
use std::net::Ipv4Addr;
use std::net::SocketAddr;
use std::num::NonZeroU16;
use std::os::fd::AsFd;
use std::sync::Arc;
use std::time::Duration;

use non_empty_string::NonEmptyString;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tokio_util::sync::DropGuard;

use crate::Error;
use crate::Session;
use crate::SessionConfig;
use crate::connection;
use crate::session;

/// Listener configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ListenerConfig {
    /// The address to listen on.
    pub bind_addr: SocketAddr,

    /// The host that search tasks connect to. When `None`, the host is the IP address of
    /// `bind_addr`, unless that address is unspecified, in which case it is the first
    /// non-loopback IPv4 address of this machine.
    pub advertised_host: Option<NonEmptyString>,

    /// How long a new connection may take to send its handshake.
    pub handshake_timeout: Duration,
}

impl Default for ListenerConfig {
    fn default() -> Self {
        Self {
            bind_addr: SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)),
            advertised_host: None,
            handshake_timeout: Duration::from_secs(10),
        }
    }
}

/// A listener that receives the results search tasks stream, and routes each connection to the
/// session named by its handshake.
///
/// Dropping the listener stops accepting connections. Sessions that are already running keep
/// serving their open connections.
pub struct ResultListener {
    sessions: Arc<session::Registry>,
    accept_barrier: AcceptBarrier,
    advertised_host: NonEmptyString,
    port: NonZeroU16,
    _accept_loop_guard: DropGuard,
}

impl ResultListener {
    /// Binds a listener and starts accepting connections in a background task.
    ///
    /// # Returns
    ///
    /// The newly bound listener on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * Forwards [`TcpListener::bind`]'s return values on failure.
    /// * Forwards [`TcpListener::local_addr`]'s return values on failure.
    /// * Forwards [`std::os::fd::BorrowedFd::try_clone_to_owned`]'s return values on failure.
    /// * Forwards `detect_advertised_ip`'s return values on failure.
    ///
    /// # Panics
    ///
    /// Panics if the bound port is zero, which the OS never assigns.
    pub async fn bind(config: ListenerConfig) -> Result<Self, Error> {
        let tcp_listener = TcpListener::bind(config.bind_addr).await?;
        let local_addr = tcp_listener.local_addr()?;
        let port = NonZeroU16::new(local_addr.port()).expect("a bound port should be nonzero");
        let queued_connections =
            std::net::TcpListener::from(tcp_listener.as_fd().try_clone_to_owned()?);
        let advertised_host = if let Some(advertised_host) = config.advertised_host {
            advertised_host
        } else {
            let ip = if local_addr.ip().is_unspecified() {
                detect_advertised_ip()?
            } else {
                local_addr.ip()
            };
            NonEmptyString::new(ip.to_string()).expect("a formatted IP address should be non-empty")
        };

        let sessions = Arc::new(session::Registry::new());
        let (barrier_request_sender, barrier_request_receiver) = mpsc::unbounded_channel();
        let (num_pending_handshakes_sender, num_pending_handshakes_receiver) = watch::channel(0);
        let shutdown = CancellationToken::new();
        let accept_loop = AcceptLoop {
            tcp_listener,
            queued_connections,
            sessions: Arc::clone(&sessions),
            num_pending_handshakes: Arc::new(num_pending_handshakes_sender),
            handshake_timeout: config.handshake_timeout,
        };
        tokio::spawn(accept_loop.run(barrier_request_receiver, shutdown.clone()));
        tracing::debug!(
            local_addr = % local_addr,
            advertised_host = % advertised_host,
            "Listening for search results."
        );

        Ok(Self {
            sessions,
            accept_barrier: AcceptBarrier {
                requests: barrier_request_sender,
                num_pending_handshakes: num_pending_handshakes_receiver,
                handshake_timeout: config.handshake_timeout,
            },
            advertised_host,
            port,
            _accept_loop_guard: shutdown.drop_guard(),
        })
    }

    #[must_use]
    pub const fn advertised_host(&self) -> &NonEmptyString {
        &self.advertised_host
    }

    #[must_use]
    pub const fn port(&self) -> NonZeroU16 {
        self.port
    }

    /// Opens a session for one query job.
    ///
    /// The session is registered before this method returns, so it accepts connections as soon
    /// as the job is submitted with the session's network output.
    ///
    /// # Returns
    ///
    /// The newly opened session.
    #[must_use]
    pub fn open_session(&self, config: SessionConfig) -> Session {
        Session::open(
            Arc::clone(&self.sessions),
            self.accept_barrier.clone(),
            self.advertised_host.clone(),
            self.port,
            config,
        )
    }
}

/// Lets a session wait until the connections accepted so far have looked up their sessions.
#[derive(Clone, Debug)]
pub struct AcceptBarrier {
    requests: mpsc::UnboundedSender<oneshot::Sender<()>>,
    num_pending_handshakes: watch::Receiver<usize>,
    handshake_timeout: Duration,
}

impl AcceptBarrier {
    /// Waits until every connection that reached the listening socket before this call has looked
    /// up the session named by its handshake, or until the handshake timeout has passed.
    ///
    /// The connections already queued on the listening socket are accepted first, so the wait
    /// covers connections the search tasks opened even if the accept loop hasn't reached them.
    pub async fn wait(&self) {
        let (reply_sender, reply_receiver) = oneshot::channel();
        // Either failure means the accept loop has stopped, so no connection is left to accept.
        if self.requests.send(reply_sender).is_ok() {
            let _ = reply_receiver.await;
        }
        let mut num_pending_handshakes = self.num_pending_handshakes.clone();
        // Each pending connection gets its handshake or is closed within the handshake timeout.
        let _ = tokio::time::timeout(
            self.handshake_timeout,
            num_pending_handshakes.wait_for(|&num_pending| 0 == num_pending),
        )
        .await;
    }
}

/// Counts a connection as waiting for its handshake until dropped.
pub struct PendingHandshake {
    num_pending_handshakes: Arc<watch::Sender<usize>>,
}

impl PendingHandshake {
    /// Factory function.
    ///
    /// Counts a newly accepted connection as waiting for its handshake.
    ///
    /// # Returns
    ///
    /// The newly created counter entry.
    fn new(num_pending_handshakes: Arc<watch::Sender<usize>>) -> Self {
        num_pending_handshakes.send_modify(|num_pending| *num_pending += 1);
        Self {
            num_pending_handshakes,
        }
    }
}

impl Drop for PendingHandshake {
    fn drop(&mut self) {
        self.num_pending_handshakes
            .send_modify(|num_pending| *num_pending -= 1);
    }
}

/// The delay before accepting again after a failed accept, so that a persistent failure such as
/// running out of file descriptors doesn't spin.
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(100);

/// The background task that accepts connections and serves each one in its own task.
struct AcceptLoop {
    tcp_listener: TcpListener,

    /// A second handle to the listening socket, for accepting the queued connections directly
    /// instead of waiting for the runtime to report the socket as readable.
    queued_connections: std::net::TcpListener,

    sessions: Arc<session::Registry>,
    num_pending_handshakes: Arc<watch::Sender<usize>>,
    handshake_timeout: Duration,
}

impl AcceptLoop {
    /// Accepts connections and answers barrier requests until `shutdown` is cancelled.
    async fn run(
        self,
        mut barrier_requests: mpsc::UnboundedReceiver<oneshot::Sender<()>>,
        shutdown: CancellationToken,
    ) {
        loop {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => return,
                Some(reply_sender) = barrier_requests.recv() => {
                    self.accept_queued_connections();
                    // A failure means the waiting session was dropped, so no one needs the reply.
                    let _ = reply_sender.send(());
                }
                accepted = self.tcp_listener.accept() => match accepted {
                    Ok((stream, peer_addr)) => self.serve(stream, peer_addr),
                    Err(e) => {
                        tracing::warn!(error = % e, "Failed to accept a connection.");
                        tokio::time::sleep(ACCEPT_RETRY_DELAY).await;
                    }
                },
            }
        }
    }

    /// Accepts every connection queued on the listening socket.
    fn accept_queued_connections(&self) {
        loop {
            let (stream, peer_addr) = match self.queued_connections.accept() {
                Ok(accepted) => accepted,
                Err(e) if ErrorKind::WouldBlock == e.kind() => return,
                Err(e) => {
                    tracing::warn!(error = % e, "Failed to accept a queued connection.");
                    return;
                }
            };
            match stream
                .set_nonblocking(true)
                .and_then(|()| TcpStream::from_std(stream))
            {
                Ok(stream) => self.serve(stream, peer_addr),
                Err(e) => tracing::warn!(
                    peer_addr = % peer_addr,
                    error = % e,
                    "Failed to register a queued connection."
                ),
            }
        }
    }

    /// Serves an accepted connection in its own task.
    fn serve(&self, stream: TcpStream, peer_addr: SocketAddr) {
        tokio::spawn(connection::serve(
            stream,
            peer_addr,
            Arc::clone(&self.sessions),
            self.handshake_timeout,
            PendingHandshake::new(Arc::clone(&self.num_pending_handshakes)),
        ));
    }
}

/// Detects the IP address to advertise to search tasks.
///
/// # Returns
///
/// The first non-loopback IPv4 address of this machine, or, if there is none, its first loopback
/// IPv4 address, on success.
///
/// # Errors
///
/// Returns an error if:
///
/// * [`Error::NoIpv4Address`] if this machine has no IPv4 address.
/// * Forwards [`if_addrs::get_if_addrs`]'s return values on failure.
fn detect_advertised_ip() -> Result<IpAddr, Error> {
    let mut loopback_ip = None;
    for interface in if_addrs::get_if_addrs()? {
        let IpAddr::V4(ip) = interface.ip() else {
            continue;
        };
        if !ip.is_loopback() {
            return Ok(IpAddr::V4(ip));
        }
        loopback_ip.get_or_insert(ip);
    }
    let ip = loopback_ip.ok_or(Error::NoIpv4Address)?;
    tracing::warn!(
        ip = % ip,
        "Couldn't find a non-loopback IPv4 address to receive search results on."
    );
    Ok(IpAddr::V4(ip))
}
