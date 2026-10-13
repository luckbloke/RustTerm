use super::{
    auth::{read_security_failure, AuthHelper, AuthResult, SecurityType},
    connection::VncClient,
    security::ra2::{self, ServerKeyVerifier, VncServerKey},
};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tracing::{info, trace};

use crate::{PixelFormat, VncEncoding, VncError, VncLimits, VncVersion};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VncSecurityPolicy {
    Auto,
    NoneOnly,
    VncAuthOnly,
}

pub enum VncState<S, F>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static,
    F: Future<Output = Result<String, VncError>> + Send + Sync + 'static,
{
    Handshake(VncConnector<S, F>),
    Authenticate(VncConnector<S, F>),
    Connected(VncClient),
}

impl<S, F> VncState<S, F>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static,
    F: Future<Output = Result<String, VncError>> + Send + Sync + 'static,
{
    pub fn try_start(
        self,
    ) -> Pin<Box<dyn Future<Output = Result<Self, VncError>> + Send + 'static>> {
        Box::pin(async move {
            match self {
                VncState::Handshake(mut connector) => {
                    // Read the rfbversion informed by the server
                    let rfbversion = VncVersion::read(&mut connector.stream).await?;
                    trace!(
                        "Our version {:?}, server version {:?}",
                        connector.rfb_version,
                        rfbversion
                    );
                    let rfbversion = if connector.rfb_version < rfbversion {
                        connector.rfb_version
                    } else {
                        rfbversion
                    };

                    // Record the negotiated rfbversion
                    connector.rfb_version = rfbversion;
                    trace!("Negotiated rfb version: {:?}", rfbversion);
                    rfbversion.write(&mut connector.stream).await?;
                    Ok(VncState::Authenticate(connector).try_start().await?)
                }
                VncState::Authenticate(mut connector) => {
                    let security_types = SecurityType::read(
                        &mut connector.stream,
                        &connector.rfb_version,
                        &connector.limits,
                    )
                    .await?;

                    let selected = match connector.security_policy {
                        VncSecurityPolicy::NoneOnly => {
                            if !security_types.contains(&SecurityType::None) {
                                return Err(VncError::RequiredSecurityTypeUnavailable("none"));
                            }
                            SecurityType::None
                        }
                        VncSecurityPolicy::VncAuthOnly => {
                            if !security_types.contains(&SecurityType::VncAuth) {
                                return Err(VncError::RequiredSecurityTypeUnavailable("vnc-auth"));
                            }
                            SecurityType::VncAuth
                        }
                        VncSecurityPolicy::Auto if connector.credentials_available => {
                            if connector.rfb_version != VncVersion::RFB33
                                && security_types.contains(&SecurityType::RA2_256)
                            {
                                SecurityType::RA2_256
                            } else if security_types.contains(&SecurityType::VncAuth) {
                                SecurityType::VncAuth
                            } else {
                                return Err(VncError::UnsupportedSecurityType);
                            }
                        }
                        VncSecurityPolicy::Auto => {
                            if security_types.contains(&SecurityType::None) {
                                SecurityType::None
                            } else {
                                return Err(VncError::UnsupportedSecurityType);
                            }
                        }
                    };

                    let client = match selected {
                        SecurityType::None => {
                            match connector.rfb_version {
                                VncVersion::RFB33 => {
                                    info!("No auth needed in vnc3.3");
                                }
                                VncVersion::RFB37 => {
                                    info!("No auth needed in vnc3.7");
                                    SecurityType::write(&selected, &mut connector.stream).await?;
                                }
                                VncVersion::RFB38 => {
                                    info!("No auth needed in vnc3.8");
                                    SecurityType::write(&selected, &mut connector.stream).await?;
                                    let result: AuthResult =
                                        connector.stream.read_u32().await?.try_into()?;
                                    if result == AuthResult::Failed {
                                        let reason = read_security_failure(
                                            &mut connector.stream,
                                            &connector.limits,
                                        )
                                        .await?;
                                        return Err(VncError::SecurityFailure(reason));
                                    }
                                }
                            }
                            VncClient::new(
                                connector.stream,
                                connector.allow_shared,
                                connector.pixel_format,
                                connector.encodings,
                                connector.limits,
                            )
                            .await?
                        }
                        SecurityType::VncAuth => {
                            let credential = connector
                                .auth_methond
                                .take()
                                .ok_or(VncError::NoPassword)?
                                .await?;
                            if credential.len() > 8 {
                                return Err(VncError::CredentialTooLong {
                                    field: "classic VNC password",
                                    actual: credential.len(),
                                    limit: 8,
                                });
                            }
                            if connector.rfb_version != VncVersion::RFB33 {
                                SecurityType::write(&selected, &mut connector.stream).await?;
                            }
                            let auth = AuthHelper::read(&mut connector.stream, &credential).await?;
                            auth.write(&mut connector.stream).await?;
                            let result = auth.finish(&mut connector.stream).await?;
                            if let AuthResult::Failed = result {
                                if let VncVersion::RFB37 = connector.rfb_version {
                                    return Err(VncError::WrongPassword);
                                }
                                let reason =
                                    read_security_failure(&mut connector.stream, &connector.limits)
                                        .await?;
                                return Err(VncError::SecurityFailure(reason));
                            }
                            VncClient::new(
                                connector.stream,
                                connector.allow_shared,
                                connector.pixel_format,
                                connector.encodings,
                                connector.limits,
                            )
                            .await?
                        }
                        SecurityType::RA2_256 => {
                            let verifier = connector
                                .server_key_verifier
                                .clone()
                                .ok_or(VncError::Ra2ServerKeyVerifierRequired)?;
                            let credential = connector
                                .auth_methond
                                .take()
                                .ok_or(VncError::NoPassword)?
                                .await?;
                            let username = connector.username.take().unwrap_or_default();
                            ra2::validate_credentials(&username, &credential)?;
                            SecurityType::write(&selected, &mut connector.stream).await?;
                            let (stream, server_key) = ra2::authenticate(
                                connector.stream,
                                username,
                                credential,
                                verifier,
                                connector.limits,
                                connector.rfb_version,
                            )
                            .await?;
                            let client = VncClient::new(
                                stream,
                                connector.allow_shared,
                                connector.pixel_format,
                                connector.encodings,
                                connector.limits,
                            )
                            .await?;
                            if let Some(notify) = connector.server_key_authenticated {
                                notify(server_key);
                            }
                            client
                        }
                        _ => return Err(VncError::UnsupportedSecurityType),
                    };
                    info!("auth done, client connected");
                    Ok(VncState::Connected(client))
                }
                VncState::Connected(_) => Err(VncError::ConnectError),
            }
        })
    }

    pub fn finish(self) -> Result<VncClient, VncError> {
        if let VncState::Connected(client) = self {
            Ok(client)
        } else {
            Err(VncError::ConnectError)
        }
    }
}

/// Connection Builder to setup a vnc client
pub struct VncConnector<S, F>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    F: Future<Output = Result<String, VncError>> + Send + Sync + 'static,
{
    stream: S,
    auth_methond: Option<F>,
    credentials_available: bool,
    username: Option<String>,
    server_key_verifier: Option<ServerKeyVerifier>,
    server_key_authenticated: Option<Arc<dyn Fn(VncServerKey) + Send + Sync>>,
    security_policy: VncSecurityPolicy,
    rfb_version: VncVersion,
    allow_shared: bool,
    pixel_format: Option<PixelFormat>,
    encodings: Vec<VncEncoding>,
    limits: VncLimits,
}

impl<S, F> VncConnector<S, F>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + Sync + 'static,
    F: Future<Output = Result<String, VncError>> + Send + Sync + 'static,
{
    /// To new a vnc client configuration with stream `S`
    ///
    /// `S` should implement async I/O methods
    ///
    /// ```no_run
    /// use vnc::{PixelFormat, VncConnector, VncError};
    /// use tokio::{self, net::TcpStream};
    ///
    /// #[tokio::main]
    /// async fn main() -> Result<(), VncError> {
    ///     let tcp = TcpStream::connect("127.0.0.1:5900").await?;
    ///     let vnc = VncConnector::new(tcp)
    ///         .set_auth_method(async move { Ok("password".to_string()) })
    ///         .add_encoding(vnc::VncEncoding::Tight)
    ///         .add_encoding(vnc::VncEncoding::Zrle)
    ///         .add_encoding(vnc::VncEncoding::CopyRect)
    ///         .add_encoding(vnc::VncEncoding::Raw)
    ///         .allow_shared(true)
    ///         .set_pixel_format(PixelFormat::bgra())
    ///         .build()?
    ///         .try_start()
    ///         .await?
    ///         .finish()?;
    ///     Ok(())
    /// }
    /// ```
    ///
    pub fn new(stream: S) -> Self {
        Self {
            stream,
            auth_methond: None,
            credentials_available: false,
            username: None,
            server_key_verifier: None,
            server_key_authenticated: None,
            security_policy: VncSecurityPolicy::Auto,
            allow_shared: true,
            rfb_version: VncVersion::RFB38,
            pixel_format: None,
            encodings: Vec::new(),
            limits: VncLimits::default(),
        }
    }

    /// An async callback which is used to query credentials if the vnc server has set
    ///
    /// ```no_compile
    /// connector = connector.set_auth_method(async move { Ok("password".to_string()) })
    /// ```
    ///
    /// if you're building a wasm app,
    /// the async callback also allows you to combine it to a promise
    ///
    /// ```no_compile
    /// #[wasm_bindgen]
    /// extern "C" {
    ///     fn get_password() -> js_sys::Promise;
    /// }
    ///
    /// connector = connector
    ///        .set_auth_method(async move {
    ///            let auth = JsFuture::from(get_password()).await.unwrap();
    ///            Ok(auth.as_string().unwrap())
    ///     });
    /// ```
    ///
    /// While in the js code
    ///
    ///
    /// ```javascript
    /// var password = '';
    /// function get_password() {
    ///     return new Promise((reslove, reject) => {
    ///        document.getElementById("submit_password").addEventListener("click", () => {
    ///             password = window.document.getElementById("input_password").value
    ///             reslove(password)
    ///         })
    ///     });
    /// }
    /// ```
    ///
    /// The future won't be polled if the sever doesn't apply any password protections to the session
    ///
    pub fn set_auth_method(mut self, auth_callback: F) -> Self {
        self.auth_methond = Some(auth_callback);
        self.credentials_available = true;
        self
    }

    pub fn set_credentials_available(mut self, available: bool) -> Self {
        self.credentials_available = available;
        self
    }

    pub fn set_username(mut self, username: impl Into<String>) -> Self {
        self.username = Some(username.into());
        self
    }

    pub fn set_server_key_verifier<V, Fut>(mut self, verifier: V) -> Self
    where
        V: Fn(VncServerKey) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), VncError>> + Send + 'static,
    {
        self.server_key_verifier = Some(Arc::new(move |key| Box::pin(verifier(key))));
        self
    }

    /// Notify the application after server-key confirmation, authenticated
    /// ServerHash, SecurityResult, and RFB initialization have all succeeded.
    /// This callback has no storage policy; applications must still guard
    /// cancellation and connection ownership before committing trust.
    pub fn set_server_key_authenticated<V>(mut self, notify: V) -> Self
    where
        V: Fn(VncServerKey) + Send + Sync + 'static,
    {
        self.server_key_authenticated = Some(Arc::new(notify));
        self
    }

    pub fn set_security_policy(mut self, policy: VncSecurityPolicy) -> Self {
        self.security_policy = policy;
        self
    }

    /// The max vnc version that we supported
    ///
    /// Version should be one of the [VncVersion]
    ///
    pub fn set_version(mut self, version: VncVersion) -> Self {
        self.rfb_version = version;
        self
    }

    /// Set the rgb order which you will use to resolve the image data
    ///
    /// In most of the case, use `PixelFormat::bgra()` on little endian PCs
    ///
    /// And use `PixelFormat::rgba()` on wasm apps (with canvas)
    ///
    /// Also, customized format is allowed
    ///
    /// Will use the default format informed by the vnc server if not set
    ///
    /// In this condition, the client will get a [crate::VncEvent::SetPixelFormat] event notified
    ///
    pub fn set_pixel_format(mut self, pf: PixelFormat) -> Self {
        self.pixel_format = Some(pf);
        self
    }

    /// Shared-flag is non-zero (true) if the server should try to share the
    ///
    /// desktop by leaving other clients connected, and zero (false) if it
    ///
    /// should give exclusive access to this client by disconnecting all
    ///
    /// other clients.
    ///
    pub fn allow_shared(mut self, allow_shared: bool) -> Self {
        self.allow_shared = allow_shared;
        self
    }

    /// Client encodings that we want to use
    ///
    /// One of [VncEncoding]
    ///
    /// [VncEncoding::Raw] must be sent as the RFC required
    ///
    /// The order to add encodings is the order to inform the server
    ///
    pub fn add_encoding(mut self, encoding: VncEncoding) -> Self {
        self.encodings.push(encoding);
        self
    }

    /// Set resource limits for server-controlled protocol fields and buffers.
    pub fn set_limits(mut self, limits: VncLimits) -> Self {
        self.limits = limits;
        self
    }

    /// Complete the client configuration
    ///
    pub fn build(self) -> Result<VncState<S, F>, VncError> {
        if self.encodings.is_empty() {
            return Err(VncError::NoEncoding);
        }
        if self.limits.channel_capacity == 0 {
            return Err(VncError::General(
                "channel capacity must be greater than zero".to_owned(),
            ));
        }
        Ok(VncState::Handshake(self))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Rect, VncEvent};
    use flate2::{write::ZlibEncoder, Compression};
    use image::{codecs::jpeg::JpegEncoder, ColorType, ImageEncoder};
    use std::io::Write;
    use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

    fn connector(
        stream: DuplexStream,
    ) -> VncState<DuplexStream, impl Future<Output = Result<String, VncError>> + Send + Sync> {
        VncConnector::new(stream)
            .set_auth_method(async { Ok("password".to_owned()) })
            .add_encoding(VncEncoding::Raw)
            .build()
            .unwrap()
    }

    fn none_connector(
        stream: DuplexStream,
    ) -> VncState<DuplexStream, impl Future<Output = Result<String, VncError>> + Send + Sync> {
        VncConnector::new(stream)
            .set_auth_method(async { Ok("password".to_owned()) })
            .set_security_policy(VncSecurityPolicy::NoneOnly)
            .add_encoding(VncEncoding::Raw)
            .build()
            .unwrap()
    }

    async fn write_server_init<S>(server: &mut S)
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        server.write_all(&2_u16.to_be_bytes()).await.unwrap();
        server.write_all(&2_u16.to_be_bytes()).await.unwrap();
        server
            .write_all(&Vec::<u8>::from(PixelFormat::default()))
            .await
            .unwrap();
        server.write_all(&4_u32.to_be_bytes()).await.unwrap();
        server.write_all(b"test").await.unwrap();
        let mut set_encodings = [0; 8];
        server.read_exact(&mut set_encodings).await.unwrap();
        assert_eq!(set_encodings, [2, 0, 0, 1, 0, 0, 0, 0]);
    }

    fn compressed_connector(
        stream: DuplexStream,
        policy: VncSecurityPolicy,
    ) -> VncState<DuplexStream, impl Future<Output = Result<String, VncError>> + Send + Sync> {
        VncConnector::new(stream)
            .set_auth_method(async { Ok("password".to_owned()) })
            .set_security_policy(policy)
            .set_pixel_format(PixelFormat::rgba())
            .add_encoding(VncEncoding::Zrle)
            .add_encoding(VncEncoding::Tight)
            .add_encoding(VncEncoding::Raw)
            .build()
            .unwrap()
    }

    fn compress(data: &[u8]) -> Vec<u8> {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(data).expect("write zlib input");
        encoder.finish().expect("finish zlib")
    }

    fn compact_len(len: usize) -> Vec<u8> {
        let mut out = vec![(len & 0x7f) as u8];
        if len > 0x7f {
            out[0] |= 0x80;
            out.push(((len >> 7) & 0x7f) as u8);
            if len > 0x3fff {
                let last = out.last_mut().expect("second byte");
                *last |= 0x80;
                out.push(((len >> 14) & 0xff) as u8);
            }
        }
        out
    }

    fn zrle_rect_payload(decoded: &[u8]) -> Vec<u8> {
        let compressed = compress(decoded);
        let mut payload = Vec::new();
        payload.extend_from_slice(&(compressed.len() as u32).to_be_bytes());
        payload.extend_from_slice(&compressed);
        payload
    }

    fn tight_jpeg_rect_payload(width: u32, height: u32, rgb: &[u8]) -> Vec<u8> {
        let mut jpeg = Vec::new();
        JpegEncoder::new(&mut jpeg)
            .write_image(rgb, width, height, ColorType::Rgb8.into())
            .expect("encode jpeg");
        let mut payload = vec![0x90];
        payload.extend_from_slice(&compact_len(jpeg.len()));
        payload.extend_from_slice(&jpeg);
        payload
    }

    fn framebuffer_update(rect: Rect, encoding: VncEncoding, body: &[u8]) -> Vec<u8> {
        let mut payload = vec![0, 0];
        payload.extend_from_slice(&1_u16.to_be_bytes());
        payload.extend_from_slice(&rect.x.to_be_bytes());
        payload.extend_from_slice(&rect.y.to_be_bytes());
        payload.extend_from_slice(&rect.width.to_be_bytes());
        payload.extend_from_slice(&rect.height.to_be_bytes());
        payload.extend_from_slice(&(encoding as i32).to_be_bytes());
        payload.extend_from_slice(body);
        payload
    }

    async fn write_scripted_server_init(server: &mut DuplexStream) {
        server.write_all(&2_u16.to_be_bytes()).await.unwrap();
        server.write_all(&2_u16.to_be_bytes()).await.unwrap();
        server
            .write_all(&Vec::<u8>::from(PixelFormat::default()))
            .await
            .unwrap();
        server.write_all(&8_u32.to_be_bytes()).await.unwrap();
        server.write_all(b"scripted").await.unwrap();

        let mut set_pixel_format = [0; 20];
        server.read_exact(&mut set_pixel_format).await.unwrap();
        assert_eq!(set_pixel_format[0..4], [0, 0, 0, 0]);
        assert_eq!(
            &set_pixel_format[4..20],
            &Vec::<u8>::from(PixelFormat::rgba())
        );

        let mut header = [0; 4];
        server.read_exact(&mut header).await.unwrap();
        assert_eq!(header, [2, 0, 0, 3]);
        let mut encodings = [0; 12];
        server.read_exact(&mut encodings).await.unwrap();
        assert_eq!(
            encodings,
            [
                0, 0, 0, 16, // ZRLE
                0, 0, 0, 7, // Tight
                0, 0, 0, 0, // Raw fallback
            ]
        );

        let mut request = [0; 10];
        server.read_exact(&mut request).await.unwrap();
        assert_eq!(request, [3, 0, 0, 0, 0, 0, 0, 2, 0, 2]);
    }

    async fn run_scripted_framebuffer(
        policy: VncSecurityPolicy,
        encoding: VncEncoding,
        rect: Rect,
        body: Vec<u8>,
    ) -> Vec<u8> {
        let (client, mut server) = tokio::io::duplex(16 * 1024);
        let server_task = tokio::spawn(async move {
            server.write_all(b"RFB 003.008\n").await.unwrap();
            let mut version = [0; 12];
            server.read_exact(&mut version).await.unwrap();

            match policy {
                VncSecurityPolicy::NoneOnly => {
                    server.write_all(&[1, 1]).await.unwrap();
                    assert_eq!(server.read_u8().await.unwrap(), 1);
                    server.write_all(&0_u32.to_be_bytes()).await.unwrap();
                }
                VncSecurityPolicy::VncAuthOnly => {
                    server.write_all(&[1, 2]).await.unwrap();
                    assert_eq!(server.read_u8().await.unwrap(), 2);
                    server.write_all(&[3; 16]).await.unwrap();
                    let mut response = [0; 16];
                    server.read_exact(&mut response).await.unwrap();
                    server.write_all(&0_u32.to_be_bytes()).await.unwrap();
                }
                VncSecurityPolicy::Auto => unreachable!("scripted tests select a fixed policy"),
            }

            let _shared = server.read_u8().await.unwrap();
            write_scripted_server_init(&mut server).await;
            server
                .write_all(&framebuffer_update(rect, encoding, &body))
                .await
                .unwrap();
        });

        let state = compressed_connector(client, policy)
            .try_start()
            .await
            .unwrap();
        let client = state.finish().unwrap();
        let mut raw = None;
        for _ in 0..4 {
            match tokio::time::timeout(std::time::Duration::from_secs(1), client.recv_event())
                .await
                .expect("event timeout")
                .unwrap()
            {
                VncEvent::RawImage(_, data) => {
                    raw = Some(data);
                    break;
                }
                VncEvent::SetResolution(_) => {}
                event => panic!("unexpected event: {event:?}"),
            }
        }
        client.close().await.unwrap();
        server_task.await.unwrap();
        raw.expect("raw image event")
    }

    async fn run_none_handshake(version: VncVersion) {
        let (client, mut server) = tokio::io::duplex(1024);
        let server_task = tokio::spawn(async move {
            server
                .write_all(<VncVersion as Into<&[u8; 12]>>::into(version))
                .await
                .unwrap();
            let mut client_version = [0; 12];
            server.read_exact(&mut client_version).await.unwrap();
            match version {
                VncVersion::RFB33 => server.write_all(&1_u32.to_be_bytes()).await.unwrap(),
                VncVersion::RFB37 | VncVersion::RFB38 => {
                    server.write_all(&[1, 1]).await.unwrap();
                    let selected = server.read_u8().await.unwrap();
                    assert_eq!(selected, 1);
                    if version == VncVersion::RFB38 {
                        server.write_all(&0_u32.to_be_bytes()).await.unwrap();
                    }
                }
            }
            let _shared = server.read_u8().await.unwrap();
            write_server_init(&mut server).await;
        });

        let state = none_connector(client).try_start().await.unwrap();
        let client = state.finish().unwrap();
        client.close().await.unwrap();
        server_task.await.unwrap();
    }

    async fn run_ra2_handshake(subtype: u8, expected_username: &'static str) {
        let (client, mut server) = tokio::io::duplex(16 * 1024);
        let limits = VncLimits::default();
        let server_task = tokio::spawn(async move {
            server.write_all(b"RFB 003.008\n").await.unwrap();
            let mut version = [0_u8; 12];
            server.read_exact(&mut version).await.unwrap();
            assert_eq!(&version, b"RFB 003.008\n");
            server.write_all(&[4, 99, 1, 2, 129]).await.unwrap();
            assert_eq!(server.read_u8().await.unwrap(), 129);

            let mut encrypted = crate::client::security::ra2::accept_test_handshake(
                server,
                subtype,
                expected_username,
                "raspberry",
                limits,
            )
            .await;
            assert_eq!(encrypted.read_u8().await.unwrap(), 1);
            write_server_init(&mut encrypted).await;
        });

        let notification_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let notified = notification_count.clone();
        let state = VncConnector::new(client)
            .set_auth_method(async { Ok("raspberry".to_owned()) })
            .set_credentials_available(true)
            .set_username("pi")
            .set_server_key_verifier(|key| async move {
                assert!(key.bits() >= 2048);
                assert!(!key.encoded().is_empty());
                Ok(())
            })
            .set_server_key_authenticated(move |key| {
                assert!(!key.encoded().is_empty());
                notified.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            })
            .set_security_policy(VncSecurityPolicy::Auto)
            .add_encoding(VncEncoding::Raw)
            .build()
            .unwrap()
            .try_start()
            .await
            .unwrap();
        assert_eq!(
            notification_count.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
        state.finish().unwrap().close().await.unwrap();
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn none_security_33_37_38() {
        run_none_handshake(VncVersion::RFB33).await;
        run_none_handshake(VncVersion::RFB37).await;
        run_none_handshake(VncVersion::RFB38).await;
    }

    #[tokio::test]
    async fn ra2_256_auto_prefers_encrypted_auth_and_supports_both_subtypes() {
        run_ra2_handshake(1, "pi").await;
        run_ra2_handshake(2, "").await;
    }

    #[tokio::test]
    async fn cancelled_or_timed_out_accepted_handshake_never_notifies_trust() {
        use rsa::traits::PublicKeyParts;
        for timeout_case in [false, true] {
            let (client, mut server) = tokio::io::duplex(16 * 1024);
            let (exchanged, exchange_ready) = tokio::sync::oneshot::channel();
            let server_task = tokio::spawn(async move {
                let key = rsa::RsaPrivateKey::new(&mut rand::rngs::OsRng, 1024).unwrap();
                server.write_all(b"RFB 003.008\n").await.unwrap();
                let mut version = [0; 12];
                server.read_exact(&mut version).await.unwrap();
                server.write_all(&[1, 129]).await.unwrap();
                assert_eq!(server.read_u8().await.unwrap(), 129);
                server.write_u32(1024).await.unwrap();
                server.write_all(&key.n().to_bytes_be()).await.unwrap();
                let mut exponent = vec![0; 128 - key.e().to_bytes_be().len()];
                exponent.extend(key.e().to_bytes_be());
                server.write_all(&exponent).await.unwrap();
                let client_bits = server.read_u32().await.unwrap();
                let mut client_key = vec![0; (client_bits as usize).div_ceil(8) * 2];
                server.read_exact(&mut client_key).await.unwrap();
                // Confirmation succeeded and key exchange started, but the
                // cryptographic server identity has not yet been authenticated.
                exchanged.send(()).unwrap();
                std::future::pending::<()>().await;
            });
            let notified = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let notification = notified.clone();
            let mut handshake = tokio::spawn(async move {
                VncConnector::new(client)
                    .set_auth_method(async { Ok("raspberry".into()) })
                    .set_server_key_verifier(|_| async { Ok(()) })
                    .set_server_key_authenticated(move |_| {
                        notification.store(true, std::sync::atomic::Ordering::SeqCst);
                    })
                    .add_encoding(VncEncoding::Raw)
                    .build()
                    .unwrap()
                    .try_start()
                    .await
            });
            exchange_ready.await.unwrap();
            if timeout_case {
                assert!(
                    tokio::time::timeout(std::time::Duration::from_millis(10), &mut handshake)
                        .await
                        .is_err()
                );
            }
            handshake.abort();
            assert!(matches!(handshake.await, Err(error) if error.is_cancelled()));
            assert!(!notified.load(std::sync::atomic::Ordering::SeqCst));
            server_task.abort();
            let _ = server_task.await;
        }
    }

    #[tokio::test]
    async fn failed_ra2_handshakes_never_notify_authenticated_key() {
        for outcome in [
            "reject",
            "hash-mismatch",
            "transport-error",
            "security-failure",
            "truncated-init",
        ] {
            let (client, mut server) = tokio::io::duplex(16 * 1024);
            let server_task = tokio::spawn(async move {
                server.write_all(b"RFB 003.008\n").await.unwrap();
                let mut version = [0; 12];
                server.read_exact(&mut version).await.unwrap();
                server.write_all(&[1, 129]).await.unwrap();
                assert_eq!(server.read_u8().await.unwrap(), 129);
                if outcome == "reject" {
                    let key = rsa::RsaPrivateKey::new(&mut rand::rngs::OsRng, 1024).unwrap();
                    use rsa::traits::PublicKeyParts;
                    server.write_u32(1024).await.unwrap();
                    server.write_all(&key.n().to_bytes_be()).await.unwrap();
                    let mut exponent = vec![0; 128 - key.e().to_bytes_be().len()];
                    exponent.extend(key.e().to_bytes_be());
                    server.write_all(&exponent).await.unwrap();
                    return;
                }
                let mut encrypted = ra2::establish_test_encryption(
                    server,
                    VncLimits::default(),
                    outcome == "hash-mismatch",
                )
                .await;
                if matches!(outcome, "hash-mismatch" | "transport-error") {
                    encrypted.flush().await.unwrap();
                    return;
                }
                encrypted.write_u8(2).await.unwrap();
                assert_eq!(encrypted.read_u8().await.unwrap(), 0);
                let len = encrypted.read_u8().await.unwrap();
                let mut password = vec![0; usize::from(len)];
                encrypted.read_exact(&mut password).await.unwrap();
                if outcome == "security-failure" {
                    encrypted.write_u32(1).await.unwrap();
                    encrypted.write_u32(6).await.unwrap();
                    encrypted.write_all(b"denied").await.unwrap();
                } else {
                    encrypted.write_u32(0).await.unwrap();
                    encrypted.flush().await.unwrap();
                    let _shared = encrypted.read_u8().await.unwrap();
                    encrypted.write_u16(2).await.unwrap();
                }
                encrypted.flush().await.unwrap();
            });
            let notified = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let notification = notified.clone();
            let result = VncConnector::new(client)
                .set_auth_method(async { Ok("raspberry".into()) })
                .set_server_key_verifier(move |_| async move {
                    if outcome == "reject" {
                        Err(VncError::Ra2ServerKeyRejected("rejected".into()))
                    } else {
                        Ok(())
                    }
                })
                .set_server_key_authenticated(move |_| {
                    notification.store(true, std::sync::atomic::Ordering::SeqCst);
                })
                .add_encoding(VncEncoding::Raw)
                .build()
                .unwrap()
                .try_start()
                .await;
            assert!(result.is_err(), "{outcome}");
            assert!(
                !notified.load(std::sync::atomic::Ordering::SeqCst),
                "{outcome}"
            );
            server_task.await.unwrap();
        }
    }

    #[tokio::test]
    async fn auto_with_credentials_never_downgrades_to_none() {
        let (client, mut server) = tokio::io::duplex(128);
        tokio::spawn(async move {
            server.write_all(b"RFB 003.008\n").await.unwrap();
            let mut version = [0_u8; 12];
            server.read_exact(&mut version).await.unwrap();
            server.write_all(&[1, 1]).await.unwrap();
        });

        let result = VncConnector::new(client)
            .set_auth_method(async { Ok("password".to_owned()) })
            .set_credentials_available(true)
            .set_security_policy(VncSecurityPolicy::Auto)
            .add_encoding(VncEncoding::Raw)
            .build()
            .unwrap()
            .try_start()
            .await;
        assert!(matches!(result, Err(VncError::UnsupportedSecurityType)));
    }

    #[tokio::test]
    async fn classic_vnc_auth_keeps_the_eight_byte_limit() {
        let (client, mut server) = tokio::io::duplex(128);
        tokio::spawn(async move {
            server.write_all(b"RFB 003.008\n").await.unwrap();
            let mut version = [0_u8; 12];
            server.read_exact(&mut version).await.unwrap();
            server.write_all(&[1, 2]).await.unwrap();
        });

        let result = VncConnector::new(client)
            .set_auth_method(async { Ok("123456789".to_owned()) })
            .set_credentials_available(true)
            .set_security_policy(VncSecurityPolicy::Auto)
            .add_encoding(VncEncoding::Raw)
            .build()
            .unwrap()
            .try_start()
            .await;
        assert!(matches!(
            result,
            Err(VncError::CredentialTooLong {
                field: "classic VNC password",
                actual: 9,
                limit: 8
            })
        ));
    }

    #[tokio::test]
    async fn scripted_rfb_none_decodes_zrle_tight_and_jpeg_as_raw() {
        let zrle = run_scripted_framebuffer(
            VncSecurityPolicy::NoneOnly,
            VncEncoding::Zrle,
            Rect {
                x: 0,
                y: 0,
                width: 2,
                height: 1,
            },
            zrle_rect_payload(&[0, 10, 20, 30, 40, 50, 60]),
        )
        .await;
        assert_eq!(zrle, &[10, 20, 30, 255, 40, 50, 60, 255]);

        let tight = run_scripted_framebuffer(
            VncSecurityPolicy::NoneOnly,
            VncEncoding::Tight,
            Rect {
                x: 0,
                y: 0,
                width: 2,
                height: 1,
            },
            vec![0, 1, 2, 3, 4, 5, 6],
        )
        .await;
        assert_eq!(tight, &[1, 2, 3, 255, 4, 5, 6, 255]);

        let jpeg = run_scripted_framebuffer(
            VncSecurityPolicy::NoneOnly,
            VncEncoding::Tight,
            Rect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            tight_jpeg_rect_payload(1, 1, &[200, 10, 20]),
        )
        .await;
        assert_eq!(jpeg.len(), 4);
    }

    #[tokio::test]
    async fn scripted_rfb_vnc_auth_decodes_zrle_tight_and_jpeg_as_raw() {
        let zrle = run_scripted_framebuffer(
            VncSecurityPolicy::VncAuthOnly,
            VncEncoding::Zrle,
            Rect {
                x: 0,
                y: 0,
                width: 2,
                height: 1,
            },
            zrle_rect_payload(&[0, 9, 8, 7, 6, 5, 4]),
        )
        .await;
        assert_eq!(zrle, &[9, 8, 7, 255, 6, 5, 4, 255]);

        let tight = run_scripted_framebuffer(
            VncSecurityPolicy::VncAuthOnly,
            VncEncoding::Tight,
            Rect {
                x: 0,
                y: 0,
                width: 2,
                height: 1,
            },
            vec![0, 6, 5, 4, 3, 2, 1],
        )
        .await;
        assert_eq!(tight, &[6, 5, 4, 255, 3, 2, 1, 255]);

        let jpeg = run_scripted_framebuffer(
            VncSecurityPolicy::VncAuthOnly,
            VncEncoding::Tight,
            Rect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            tight_jpeg_rect_payload(1, 1, &[20, 100, 200]),
        )
        .await;
        assert_eq!(jpeg.len(), 4);
    }

    #[tokio::test]
    async fn none_security_38_rejects_unknown_result() {
        let (client, mut server) = tokio::io::duplex(128);
        tokio::spawn(async move {
            server.write_all(b"RFB 003.008\n").await.unwrap();
            let mut version = [0; 12];
            server.read_exact(&mut version).await.unwrap();
            server.write_all(&[1, 1]).await.unwrap();
            assert_eq!(server.read_u8().await.unwrap(), 1);
            server.write_all(&7_u32.to_be_bytes()).await.unwrap();
        });
        let error = none_connector(client).try_start().await.err().unwrap();
        assert!(matches!(error, VncError::InvalidSecurityResult(7)));
    }

    #[tokio::test]
    async fn vnc_auth_38_success() {
        let (client, mut server) = tokio::io::duplex(1024);
        let server_task = tokio::spawn(async move {
            server.write_all(b"RFB 003.008\n").await.unwrap();
            let mut version = [0; 12];
            server.read_exact(&mut version).await.unwrap();
            server.write_all(&[1, 2]).await.unwrap();
            assert_eq!(server.read_u8().await.unwrap(), 2);
            server.write_all(&[3; 16]).await.unwrap();
            let mut response = [0; 16];
            server.read_exact(&mut response).await.unwrap();
            server.write_all(&0_u32.to_be_bytes()).await.unwrap();
            let _shared = server.read_u8().await.unwrap();
            write_server_init(&mut server).await;
        });
        let state = connector(client).try_start().await.unwrap();
        state.finish().unwrap().close().await.unwrap();
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn required_security_policy_is_fail_closed() {
        let (client, mut server) = tokio::io::duplex(128);
        tokio::spawn(async move {
            server.write_all(b"RFB 003.008\n").await.unwrap();
            let mut version = [0; 12];
            server.read_exact(&mut version).await.unwrap();
            server.write_all(&[1, 1]).await.unwrap();
        });

        let result = VncConnector::new(client)
            .set_auth_method(async { Ok("password".to_owned()) })
            .set_security_policy(VncSecurityPolicy::VncAuthOnly)
            .add_encoding(VncEncoding::Raw)
            .build()
            .unwrap()
            .try_start()
            .await;
        let Err(error) = result else {
            panic!("required VNC auth policy must fail when the server only offers None");
        };

        assert!(matches!(
            error,
            VncError::RequiredSecurityTypeUnavailable("vnc-auth")
        ));
    }
}
