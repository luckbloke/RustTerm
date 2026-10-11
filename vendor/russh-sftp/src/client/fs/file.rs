use std::{
    collections::VecDeque,
    fmt,
    future::{self, Future},
    io::{self, SeekFrom},
    pin::Pin,
    sync::Arc,
    task::{ready, Context, Poll},
};
use tokio::io::{AsyncRead, AsyncSeek, AsyncWrite, ReadBuf};

use super::Metadata;
use crate::{
    client::{
        error::Error,
        rawsession::{PendingRequest, SftpResult},
        session::Features,
        RawSftpSession,
    },
    protocol::{Packet, StatusCode},
};

type StateFn<T> = Option<Pin<Box<dyn Future<Output = io::Result<T>> + Send + Sync + 'static>>>;

// read packet overhead: type(1) + id(4) + data_len(4)
const READ_OVERHEAD_LENGTH: u32 = 9;
// write packet overhead excluding handle: type(1) + id(4) + handle_len(4) + offset(8) + data_len(4)
const WRITE_OVERHEAD_LENGTH: u32 = 21;

#[derive(Debug)]
struct SftpIoError {
    message: String,
    source: Error,
}

impl fmt::Display for SftpIoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for SftpIoError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

fn io_error_with_sftp_source(kind: io::ErrorKind, message: String, source: Error) -> io::Error {
    io::Error::new(kind, SftpIoError { message, source })
}

fn sftp_error_to_io(error: Error) -> io::Error {
    let message = error.to_string();
    io_error_with_sftp_source(io::ErrorKind::Other, message, error)
}

struct FileState {
    f_read: StateFn<Option<Vec<u8>>>,
    f_seek: StateFn<u64>,
    f_flush: StateFn<()>,
    f_shutdown: StateFn<()>,
    write_acks: VecDeque<PendingRequest>,
}

/// Provides high-level methods for interaction with a remote file.
///
/// In order to properly close the handle, [`shutdown`] on a file should be called.
/// Also implement [`AsyncSeek`] and other async i/o implementations.
///
/// # Weakness
/// Using [`SeekFrom::End`] is costly and time-consuming because we need to
/// request the actual file size from the remote server.
pub struct File {
    session: Arc<RawSftpSession>,
    handle: String,
    state: FileState,
    pos: u64,
    closed: bool,
    features: Features,
}

impl File {
    pub(crate) fn new(session: Arc<RawSftpSession>, handle: String, features: Features) -> Self {
        Self {
            session,
            handle,
            state: FileState {
                f_read: None,
                f_seek: None,
                f_flush: None,
                f_shutdown: None,
                write_acks: VecDeque::with_capacity(features.max_concurrent_writes),
            },
            pos: 0,
            closed: false,
            features,
        }
    }

    /// Queries metadata about the remote file.
    pub async fn metadata(&self) -> SftpResult<Metadata> {
        Ok(self.session.fstat(self.handle.as_str()).await?.attrs)
    }

    /// Sets metadata for a remote file.
    pub async fn set_metadata(&self, metadata: Metadata) -> SftpResult<()> {
        self.session
            .fsetstat(self.handle.as_str(), metadata)
            .await
            .map(|_| ())
    }

    /// Attempts to sync all data.
    ///
    /// If the server does not support `fsync@openssh.com` sending the request will
    /// be omitted, but will still pseudo-successfully
    pub async fn sync_all(&self) -> SftpResult<()> {
        if !self.features.fsync {
            return Ok(());
        }

        self.session.fsync(self.handle.as_str()).await.map(|_| ())
    }

    /// Reads data at `offset` without changing the sequential stream position.
    ///
    /// The returned buffer may be shorter than `len` at EOF or when constrained
    /// by server packet/read limits.
    pub async fn read_at(&self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        let max_read_len = self
            .features
            .limits
            .and_then(|l| l.read_len)
            .unwrap_or_else(|| {
                self.features
                    .max_packet_len
                    .saturating_sub(READ_OVERHEAD_LENGTH) as u64
            }) as usize;
        let len = len.min(max_read_len);
        if len == 0 {
            return Ok(Vec::new());
        }

        match self
            .session
            .read(self.handle.clone(), offset, len as u32)
            .await
        {
            Ok(data) => Ok(data.data),
            Err(Error::Status(status)) if status.status_code == StatusCode::Eof => Ok(Vec::new()),
            Err(e) => Err(io::Error::other(e.to_string())),
        }
    }
}

fn check_write_result(result: SftpResult<Packet>) -> io::Result<()> {
    match result {
        Ok(Packet::Status(s)) if s.status_code == StatusCode::Ok => Ok(()),
        Ok(Packet::Status(s)) => {
            let message = s.error_message.clone();
            Err(io_error_with_sftp_source(
                io::ErrorKind::Other,
                message,
                Error::Status(s),
            ))
        }
        Ok(_) => Err(io_error_with_sftp_source(
            io::ErrorKind::Other,
            "unexpected response packet".to_string(),
            Error::UnexpectedPacket,
        )),
        Err(Error::Timeout) => Err(io_error_with_sftp_source(
            io::ErrorKind::TimedOut,
            "SFTP write acknowledgement timed out".to_string(),
            Error::Timeout,
        )),
        Err(e) => Err(sftp_error_to_io(e)),
    }
}

fn poll_oldest_write(
    pending: &mut VecDeque<PendingRequest>,
    cx: &mut Context<'_>,
) -> Option<Poll<io::Result<()>>> {
    let rx = pending.front_mut()?;
    Some(match Pin::new(rx).poll(cx) {
        Poll::Pending => Poll::Pending,
        Poll::Ready(r) => {
            pending.pop_front();
            Poll::Ready(check_write_result(r))
        }
    })
}

fn poll_drain_writes(
    pending: &mut VecDeque<PendingRequest>,
    cx: &mut Context<'_>,
) -> Poll<io::Result<()>> {
    while let Some(poll) = poll_oldest_write(pending, cx) {
        ready!(poll)?;
    }
    Poll::Ready(Ok(()))
}

impl Drop for File {
    fn drop(&mut self) {
        if self.closed {
            return;
        }

        let handle = std::mem::take(&mut self.handle);
        if !handle.is_empty() {
            self.session.close_detached(handle);
        }
    }
}

impl AsyncRead for File {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let poll = Pin::new(match self.state.f_read.as_mut() {
            Some(f) => f,
            None => {
                let session = self.session.clone();
                let max_read_len = self
                    .features
                    .limits
                    .and_then(|l| l.read_len)
                    .unwrap_or_else(|| {
                        self.features
                            .max_packet_len
                            .saturating_sub(READ_OVERHEAD_LENGTH) as u64
                    }) as usize;

                let file_handle = self.handle.clone();

                let offset = self.pos;
                let len = usize::min(buf.remaining(), max_read_len);

                self.state.f_read.get_or_insert(Box::pin(async move {
                    let result = session.read(file_handle, offset, len as u32).await;
                    match result {
                        Ok(data) => Ok(Some(data.data)),
                        Err(Error::Status(status)) if status.status_code == StatusCode::Eof => {
                            Ok(None)
                        }
                        Err(e) => Err(io::Error::other(e.to_string())),
                    }
                }))
            }
        })
        .poll(cx);

        if poll.is_ready() {
            self.state.f_read = None;
        }

        match poll {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Ready(Ok(None)) => Poll::Ready(Ok(())),
            Poll::Ready(Ok(Some(data))) => {
                self.pos += data.len() as u64;
                buf.put_slice(&data[..]);
                Poll::Ready(Ok(()))
            }
        }
    }
}

impl AsyncSeek for File {
    fn start_seek(mut self: Pin<&mut Self>, position: io::SeekFrom) -> io::Result<()> {
        if self.state.f_seek.is_some() {
            return Err(io::Error::other(
                "other file operation is pending, call poll_complete before start_seek",
            ));
        }

        self.state.f_seek = Some(match position {
            SeekFrom::Start(pos) => Box::pin(future::ready(Ok(pos))),
            SeekFrom::Current(pos) => {
                let new_pos = self.pos as i64 + pos;
                if new_pos < 0 {
                    return Err(io::Error::other(
                        "cannot move file pointer before the beginning",
                    ));
                }
                Box::pin(future::ready(Ok(new_pos as u64)))
            }
            SeekFrom::End(pos) => {
                let session = self.session.clone();
                let file_handle = self.handle.clone();

                Box::pin(async move {
                    let result = session
                        .fstat(file_handle)
                        .await
                        .map_err(|e| io::Error::other(e.to_string()))?;
                    match result.attrs.size {
                        Some(size) => {
                            let new_pos = size as i64 + pos;
                            if new_pos < 0 {
                                return Err(io::Error::other(
                                    "cannot move file pointer before the beginning",
                                ));
                            }
                            Ok(new_pos as u64)
                        }
                        None => Err(io::Error::other("file size unknown")),
                    }
                })
            }
        });

        Ok(())
    }

    fn poll_complete(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        match self.state.f_seek.as_mut() {
            None => Poll::Ready(Ok(self.pos)),
            Some(f) => {
                self.pos = ready!(Pin::new(f).poll(cx))?;
                self.state.f_seek = None;
                Poll::Ready(Ok(self.pos))
            }
        }
    }
}

impl AsyncWrite for File {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        if self.state.write_acks.len() >= self.features.max_concurrent_writes {
            if let Some(poll) = poll_oldest_write(&mut self.state.write_acks, cx) {
                ready!(poll)?;
            }
        }

        let max_write_len = self
            .features
            .limits
            .and_then(|l| l.write_len)
            .unwrap_or_else(|| {
                let overhead = WRITE_OVERHEAD_LENGTH + self.handle.len() as u32;
                self.features.max_packet_len.saturating_sub(overhead) as u64
            }) as usize;

        let len = usize::min(buf.len(), max_write_len);
        let data = buf[..len].to_vec();
        let handle = self.handle.clone();
        let offset = self.pos;

        match self.session.write_nowait(handle, offset, data) {
            Ok(rx) => {
                self.pos += len as u64;
                self.state.write_acks.push_back(rx);
                Poll::Ready(Ok(len))
            }
            Err(e) => Poll::Ready(Err(sftp_error_to_io(e))),
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        ready!(poll_drain_writes(&mut self.state.write_acks, cx))?;

        if !self.features.fsync {
            return Poll::Ready(Ok(()));
        }

        let poll = Pin::new(match self.state.f_flush.as_mut() {
            Some(f) => f,
            None => {
                let session = self.session.clone();
                let file_handle = self.handle.clone();

                self.state.f_flush.get_or_insert(Box::pin(async move {
                    session
                        .fsync(file_handle)
                        .await
                        .map(|_| ())
                        .map_err(sftp_error_to_io)
                }))
            }
        })
        .poll(cx);

        if poll.is_ready() {
            self.state.f_flush = None;
        }

        poll
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        if self.closed {
            return Poll::Ready(Ok(()));
        }

        ready!(poll_drain_writes(&mut self.state.write_acks, cx))?;

        let poll = Pin::new(match self.state.f_shutdown.as_mut() {
            Some(f) => f,
            None => {
                let session = self.session.clone();
                let file_handle = self.handle.clone();

                self.state.f_shutdown.get_or_insert(Box::pin(async move {
                    session.close(file_handle).await.map_err(sftp_error_to_io)?;
                    Ok(())
                }))
            }
        })
        .poll(cx);

        if poll.is_ready() {
            self.state.f_shutdown = None;
            self.closed = true;
            self.handle.clear();
        }

        poll
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::{Config, RawSftpSession};
    use crate::protocol::{FileAttributes, Handle, OpenFlags, Packet, StatusCode, Version};
    use std::error::Error as StdError;
    use std::time::Duration;
    use tokio::io::{AsyncWriteExt, DuplexStream};

    fn sftp_error_source(error: &io::Error) -> Option<&Error> {
        let mut source = error.source();
        while let Some(current) = source {
            if let Some(error) = current.downcast_ref::<Error>() {
                return Some(error);
            }
            source = current.source();
        }
        None
    }

    async fn read_client_packet(server: &mut DuplexStream) -> Packet {
        let mut bytes = crate::utils::read_packet(server, u32::MAX)
            .await
            .expect("server should read client packet");
        Packet::try_from(&mut bytes).expect("client packet should decode")
    }

    async fn write_server_packet(server: &mut DuplexStream, packet: Packet) {
        let bytes = bytes::Bytes::try_from(packet).expect("server packet should encode");
        server
            .write_all(&bytes)
            .await
            .expect("server should write packet");
    }

    async fn initialize_session(session: &RawSftpSession, server: &mut DuplexStream) {
        let client = session.init();
        let server = async {
            match read_client_packet(server).await {
                Packet::Init(_) => {}
                packet => panic!("expected init packet, got {packet:?}"),
            }
            write_server_packet(server, Version::new().into()).await;
        };

        let (client_result, _) = tokio::join!(client, server);
        client_result.expect("session should initialize");
    }

    async fn open_test_handle(session: &RawSftpSession, server: &mut DuplexStream) -> String {
        let client = session.open("remote.txt", OpenFlags::READ, FileAttributes::empty());
        let server = async {
            let id = match read_client_packet(server).await {
                Packet::Open(open) => open.id,
                packet => panic!("expected open packet, got {packet:?}"),
            };
            write_server_packet(
                server,
                Handle {
                    id,
                    handle: "drop-handle".to_string(),
                }
                .into(),
            )
            .await;
        };

        let (client_result, _) = tokio::join!(client, server);
        client_result.expect("open should succeed").handle
    }

    fn test_features() -> Features {
        Features {
            hardlink: false,
            fsync: false,
            statvfs: false,
            limits: None,
            max_concurrent_writes: 8,
            max_packet_len: 262_144,
        }
    }

    #[tokio::test]
    async fn shutdown_times_out_when_write_ack_never_arrives() {
        let (client, _server) = tokio::io::duplex(4096);
        let session = Arc::new(RawSftpSession::new_with_config(
            client,
            Config {
                request_timeout_secs: 0,
                ..Config::default()
            },
        ));
        let mut file = File::new(session, "handle".to_string(), test_features());

        file.write_all(b"payload")
            .await
            .expect("write should queue without waiting for ack");
        let error = file
            .shutdown()
            .await
            .expect_err("shutdown should drain pending write ack and time out");

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(error.to_string(), "SFTP write acknowledgement timed out");
        assert!(matches!(sftp_error_source(&error), Some(Error::Timeout)));
    }

    #[test]
    fn write_status_preserves_message_kind_and_sftp_source() {
        let error = check_write_result(Ok(Packet::status(
            1,
            StatusCode::PermissionDenied,
            "write denied",
            "en-US",
        )))
        .expect_err("non-OK write status should fail");

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(error.to_string(), "write denied");
        assert!(matches!(
            sftp_error_source(&error),
            Some(Error::Status(status)) if status.status_code == StatusCode::PermissionDenied
        ));
    }

    #[tokio::test]
    async fn shutdown_status_preserves_message_kind_and_sftp_source() {
        let (client, mut server) = tokio::io::duplex(4096);
        let session = Arc::new(RawSftpSession::new_with_config(
            client,
            Config {
                request_timeout_secs: 10,
                ..Config::default()
            },
        ));
        initialize_session(&session, &mut server).await;
        let handle = open_test_handle(&session, &mut server).await;
        let mut file = File::new(session, handle.clone(), test_features());

        let client = file.shutdown();
        let server = async {
            let close_id = match read_client_packet(&mut server).await {
                Packet::Close(close) => {
                    assert_eq!(close.handle, handle);
                    close.id
                }
                packet => panic!("expected close packet, got {packet:?}"),
            };
            write_server_packet(
                &mut server,
                Packet::status(
                    close_id,
                    StatusCode::PermissionDenied,
                    "close denied",
                    "en-US",
                ),
            )
            .await;
        };

        let (result, _) = tokio::join!(client, server);
        let error = result.expect_err("non-OK close status should fail");

        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(error.to_string(), "Permission denied: close denied");
        assert!(matches!(
            sftp_error_source(&error),
            Some(Error::Status(status)) if status.status_code == StatusCode::PermissionDenied
        ));
    }

    #[tokio::test]
    async fn drop_tracks_detached_close_until_status_is_consumed() {
        let (client, mut server) = tokio::io::duplex(4096);
        let session = Arc::new(RawSftpSession::new_with_config(
            client,
            Config {
                request_timeout_secs: 10,
                ..Config::default()
            },
        ));
        initialize_session(&session, &mut server).await;
        let handle = open_test_handle(&session, &mut server).await;
        assert_eq!(session.open_handle_count(), 1);

        let file = File::new(session.clone(), handle.clone(), test_features());
        drop(file);

        let close_id = match read_client_packet(&mut server).await {
            Packet::Close(close) => {
                assert_eq!(close.handle, handle);
                close.id
            }
            packet => panic!("expected close packet, got {packet:?}"),
        };
        write_server_packet(
            &mut server,
            Packet::status(close_id, StatusCode::Ok, "Ok", "en-US"),
        )
        .await;

        tokio::time::timeout(Duration::from_secs(1), async {
            while session.open_handle_count() != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("detached close should consume status and release handle");

        assert_eq!(session.open_handle_count(), 0);
    }
}
