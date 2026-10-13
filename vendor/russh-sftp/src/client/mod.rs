pub mod error;
pub mod fs;
mod handler;
pub mod rawsession;
pub(crate) mod runtime;
mod session;

pub use handler::Handler;
pub use rawsession::RawSftpSession;
pub use session::SftpSession;

use bytes::Bytes;
use std::sync::Arc;
use tokio::{
    io::{self, AsyncRead, AsyncWrite, AsyncWriteExt},
    select,
    sync::mpsc,
};
use tokio_util::sync::CancellationToken;

use crate::{error::Error as StreamError, protocol::Packet, utils::read_packet};

macro_rules! into_wrap {
    ($handler:expr) => {
        match $handler.await {
            Err(error) => Err(error.into()),
            Ok(()) => Ok(()),
        }
    };
}

#[derive(Clone, Debug)]
pub struct Config {
    /// Maximum size of a single packet in bytes. Default: 256 KiB.
    pub max_packet_len: u32,
    /// Maximum number of concurrent in-flight write requests. Default: 8.
    pub max_concurrent_writes: usize,
    /// Timeout in seconds for each request. Default: 10.
    pub request_timeout_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_packet_len: 262144,
            max_concurrent_writes: 8,
            request_timeout_secs: 10,
        }
    }
}

async fn execute_handler<H>(bytes: &mut Bytes, handler: &mut H) -> Result<(), error::Error>
where
    H: Handler + Send,
{
    match Packet::try_from(bytes)? {
        Packet::Version(p) => into_wrap!(handler.version(p)),
        Packet::Status(p) => into_wrap!(handler.status(p)),
        Packet::Handle(p) => into_wrap!(handler.handle(p)),
        Packet::Data(p) => into_wrap!(handler.data(p)),
        Packet::Name(p) => into_wrap!(handler.name(p)),
        Packet::Attrs(p) => into_wrap!(handler.attrs(p)),
        Packet::ExtendedReply(p) => into_wrap!(handler.extended_reply(p)),
        _ => Err(error::Error::UnexpectedBehavior(
            "A packet was received that could not be processed.".to_owned(),
        )),
    }
}

async fn process_handler<S, H>(stream: &mut S, handler: &mut H) -> Result<(), StreamError>
where
    S: AsyncRead + Unpin,
    H: Handler + Send,
{
    let mut bytes = read_packet(stream, u32::MAX).await?;
    Ok(execute_handler(&mut bytes, handler).await?)
}

/// Run processing stream as SFTP client. Is a simple handler of incoming
/// and outgoing packets. Can be used for non-standard implementations
pub fn run<S, H>(
    stream: S,
    mut handler: H,
    fail_pending: Arc<dyn Fn(error::Error) + Send + Sync + 'static>,
) -> mpsc::UnboundedSender<Bytes>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    H: Handler + Send + 'static,
{
    let (tx, mut rx) = mpsc::unbounded_channel::<Bytes>();
    let (mut rd, mut wr) = io::split(stream);

    let rc = CancellationToken::new();
    let wc = rc.clone();
    {
        let fail_pending = fail_pending.clone();
        runtime::spawn(async move {
            loop {
                select! {
                    result = process_handler(&mut rd, &mut handler) => {
                        match result {
                            Err(StreamError::UnexpectedEof) => {
                                fail_pending(error::Error::UnexpectedBehavior(
                                    "SFTP stream closed".to_owned(),
                                ));
                                break;
                            }
                            Err(err) => {
                                warn!("{}", err);
                                fail_pending(error::Error::UnexpectedBehavior(format!(
                                    "SFTP stream failed: {err}",
                                )));
                                break;
                            }
                            Ok(_) => (),
                        }
                    },
                    _ = rc.cancelled() => break,
                }
            }

            rc.cancel();
            debug!("read half of sftp stream ended");
        });
    }

    runtime::spawn(async move {
        loop {
            select! {
                Some(data) = rx.recv() => {
                    if data.is_empty() {
                        let _ = wr.shutdown().await;
                        break;
                    }

                    if let Err(error) = wr.write_all(&data[..]).await {
                        fail_pending(error::Error::from(error));
                        wc.cancel();
                        break;
                    }
                },
                _ = wc.cancelled() => break,
            }
        }

        wc.cancel();
        debug!("write half of sftp stream ended");
    });

    tx
}
