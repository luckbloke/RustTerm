use std::{
    future::Future,
    io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use aes::Aes256;
use eax::{
    aead::{generic_array::GenericArray, AeadInPlace, KeyInit},
    Eax,
};
use rand::{rngs::OsRng, RngCore};
use rsa::{traits::PublicKeyParts, BigUint, Pkcs1v15Encrypt, RsaPrivateKey, RsaPublicKey};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};

use crate::{VncError, VncLimits, VncVersion};

use super::super::auth::read_security_failure;

type Aes256Eax = Eax<Aes256>;
const RA2_RANDOM_BYTES: usize = 16;
const RA2_HASH_BYTES: usize = 32;
const RA2_MAC_BYTES: usize = 16;
const CLIENT_RSA_BITS: usize = 2048;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VncServerKey {
    bits: u32,
    encoded: Vec<u8>,
}

impl VncServerKey {
    pub fn bits(&self) -> u32 {
        self.bits
    }

    pub fn encoded(&self) -> &[u8] {
        &self.encoded
    }
}

pub(crate) type ServerKeyVerifyFuture =
    Pin<Box<dyn Future<Output = Result<(), VncError>> + Send + 'static>>;
pub(crate) type ServerKeyVerifier =
    Arc<dyn Fn(VncServerKey) -> ServerKeyVerifyFuture + Send + Sync + 'static>;

pub(crate) fn validate_credentials(username: &str, password: &str) -> Result<(), VncError> {
    validate_credential("RA2 username", username)?;
    validate_credential("RA2 password", password)
}

fn validate_credential(field: &'static str, value: &str) -> Result<(), VncError> {
    let len = value.len();
    if len > usize::from(u8::MAX) {
        return Err(VncError::CredentialTooLong {
            field,
            actual: len,
            limit: usize::from(u8::MAX),
        });
    }
    Ok(())
}

pub(crate) async fn authenticate<S>(
    mut stream: S,
    username: String,
    password: String,
    verifier: ServerKeyVerifier,
    limits: VncLimits,
    version: VncVersion,
) -> Result<(Ra2Stream<S>, VncServerKey), VncError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    validate_credentials(&username, &password)?;

    let (server_public_key, server_wire) = read_public_key(&mut stream, &limits).await?;
    let server_key = VncServerKey {
        bits: u32::try_from(server_public_key.n().bits()).unwrap_or(u32::MAX),
        encoded: server_wire.clone(),
    };
    verifier(server_key.clone()).await?;

    let mut rng = OsRng;
    let client_private_key = RsaPrivateKey::new(&mut rng, CLIENT_RSA_BITS)
        .map_err(|_| VncError::Ra2Crypto("failed to generate client RSA key"))?;
    let client_public_key = RsaPublicKey::from(&client_private_key);
    let client_wire = encode_public_key(&client_public_key)?;
    stream.write_all(&client_wire).await?;

    let encrypted_server_random_len = usize::from(stream.read_u16().await?);
    let expected_server_random_len = client_private_key.size();
    if encrypted_server_random_len != expected_server_random_len {
        return Err(VncError::InvalidRa2EncryptedRandomLength {
            actual: encrypted_server_random_len,
            expected: expected_server_random_len,
        });
    }
    let mut encrypted_server_random = vec![0; encrypted_server_random_len];
    stream.read_exact(&mut encrypted_server_random).await?;
    let server_random = client_private_key
        .decrypt(Pkcs1v15Encrypt, &encrypted_server_random)
        .map_err(|_| VncError::Ra2Crypto("failed to decrypt server random"))?;
    // The published RFB extension describes 16-byte randoms. TigerVNC
    // RA2_256 uses 32 bytes (AES key size / 8); mirror the server length
    // to interoperate with both without relaxing RSA or record verification.
    if !matches!(server_random.len(), RA2_RANDOM_BYTES | 32) {
        return Err(VncError::InvalidRa2RandomLength(server_random.len()));
    }

    let mut client_random = vec![0_u8; server_random.len()];
    rng.fill_bytes(&mut client_random);
    let encrypted_client_random = server_public_key
        .encrypt(&mut rng, Pkcs1v15Encrypt, &client_random)
        .map_err(|_| VncError::Ra2Crypto("failed to encrypt client random"))?;
    let encrypted_client_random_len =
        u16::try_from(encrypted_client_random.len()).map_err(|_| VncError::LimitExceeded {
            field: "RA2 encrypted client random",
            actual: encrypted_client_random.len() as u64,
            limit: u64::from(u16::MAX),
        })?;
    stream.write_u16(encrypted_client_random_len).await?;
    stream.write_all(&encrypted_client_random).await?;

    let client_session_key = sha256_concat(&server_random, &client_random);
    let server_session_key = sha256_concat(&client_random, &server_random);
    let expected_server_hash = sha256_concat(&server_wire, &client_wire);
    let client_hash = sha256_concat(&client_wire, &server_wire);

    let mut encrypted = Ra2Stream::new(
        stream,
        client_session_key,
        server_session_key,
        limits.max_ra2_record_bytes,
    )?;

    let mut server_hash = [0_u8; RA2_HASH_BYTES];
    encrypted.read_exact(&mut server_hash).await?;
    if server_hash != expected_server_hash {
        return Err(VncError::Ra2ServerHashMismatch);
    }
    encrypted.write_all(&client_hash).await?;

    let subtype = encrypted.read_u8().await?;
    let mut credentials = Vec::with_capacity(username.len() + password.len() + 2);
    match subtype {
        1 => {
            credentials.push(username.len() as u8);
            credentials.extend_from_slice(username.as_bytes());
        }
        2 => credentials.push(0),
        invalid => return Err(VncError::InvalidRa2Subtype(invalid)),
    }
    credentials.push(password.len() as u8);
    credentials.extend_from_slice(password.as_bytes());
    encrypted.write_all(&credentials).await?;

    let result = encrypted.read_u32().await?;
    match result {
        0 => Ok((encrypted, server_key)),
        1 => {
            if version == VncVersion::RFB38 {
                let reason = read_security_failure(&mut encrypted, &limits).await?;
                Err(VncError::SecurityFailure(reason))
            } else {
                Err(VncError::WrongPassword)
            }
        }
        invalid => Err(VncError::InvalidSecurityResult(invalid)),
    }
}

async fn read_public_key<S>(
    stream: &mut S,
    limits: &VncLimits,
) -> Result<(RsaPublicKey, Vec<u8>), VncError>
where
    S: AsyncRead + Unpin,
{
    let bits = stream.read_u32().await?;
    if bits < limits.min_ra2_key_bits || bits > limits.max_ra2_key_bits {
        return Err(VncError::InvalidRa2KeyLength {
            actual: bits,
            min: limits.min_ra2_key_bits,
            max: limits.max_ra2_key_bits,
        });
    }
    let key_bytes_u32 = bits
        .checked_add(7)
        .ok_or(VncError::IntegerOverflow("RA2 key byte length"))?
        / 8;
    let key_bytes = usize::try_from(key_bytes_u32)
        .map_err(|_| VncError::IntegerOverflow("RA2 key byte length"))?;
    let mut modulus = vec![0_u8; key_bytes];
    let mut exponent = vec![0_u8; key_bytes];
    stream.read_exact(&mut modulus).await?;
    stream.read_exact(&mut exponent).await?;

    let public_key = RsaPublicKey::new_with_max_size(
        BigUint::from_bytes_be(&modulus),
        BigUint::from_bytes_be(&exponent),
        usize::try_from(limits.max_ra2_key_bits)
            .map_err(|_| VncError::IntegerOverflow("RA2 maximum key length"))?,
    )
    .map_err(|_| VncError::InvalidRa2PublicKey)?;

    let actual_bits = u32::try_from(public_key.n().bits()).unwrap_or(u32::MAX);
    if actual_bits < limits.min_ra2_key_bits || actual_bits > limits.max_ra2_key_bits {
        return Err(VncError::InvalidRa2KeyLength {
            actual: actual_bits,
            min: limits.min_ra2_key_bits,
            max: limits.max_ra2_key_bits,
        });
    }

    let mut wire = Vec::with_capacity(4 + key_bytes * 2);
    wire.extend_from_slice(&bits.to_be_bytes());
    wire.extend_from_slice(&modulus);
    wire.extend_from_slice(&exponent);
    Ok((public_key, wire))
}

fn encode_public_key(key: &RsaPublicKey) -> Result<Vec<u8>, VncError> {
    let bits = u32::try_from(key.n().bits()).map_err(|_| VncError::InvalidRa2PublicKey)?;
    let key_bytes = usize::try_from(
        bits.checked_add(7)
            .ok_or(VncError::IntegerOverflow("RA2 key byte length"))?
            / 8,
    )
    .map_err(|_| VncError::IntegerOverflow("RA2 key byte length"))?;
    let modulus = key.n().to_bytes_be();
    let exponent = key.e().to_bytes_be();
    if modulus.len() > key_bytes || exponent.len() > key_bytes {
        return Err(VncError::InvalidRa2PublicKey);
    }

    let mut wire = Vec::with_capacity(4 + key_bytes * 2);
    wire.extend_from_slice(&bits.to_be_bytes());
    wire.resize(4 + key_bytes - modulus.len(), 0);
    wire.extend_from_slice(&modulus);
    wire.resize(4 + key_bytes * 2 - exponent.len(), 0);
    wire.extend_from_slice(&exponent);
    Ok(wire)
}

fn sha256_concat(left: &[u8], right: &[u8]) -> [u8; RA2_HASH_BYTES] {
    let mut hasher = Sha256::new();
    hasher.update(left);
    hasher.update(right);
    hasher.finalize().into()
}

pub(crate) struct Ra2Stream<S> {
    stream: S,
    write_cipher: Aes256Eax,
    read_cipher: Aes256Eax,
    write_counter: [u8; 16],
    read_counter: [u8; 16],
    max_record_bytes: usize,
    read_header: [u8; 2],
    read_header_pos: usize,
    read_body: Vec<u8>,
    read_body_pos: usize,
    read_plain: Vec<u8>,
    read_plain_pos: usize,
    write_pending: Vec<u8>,
    write_pending_pos: usize,
}

impl<S> Ra2Stream<S> {
    fn new(
        stream: S,
        write_key: [u8; 32],
        read_key: [u8; 32],
        max_record_bytes: usize,
    ) -> Result<Self, VncError> {
        if max_record_bytes == 0 || max_record_bytes > usize::from(u16::MAX) {
            return Err(VncError::InvalidRa2RecordLimit(max_record_bytes));
        }
        let write_cipher = Aes256Eax::new_from_slice(&write_key)
            .map_err(|_| VncError::Ra2Crypto("invalid client session key"))?;
        let read_cipher = Aes256Eax::new_from_slice(&read_key)
            .map_err(|_| VncError::Ra2Crypto("invalid server session key"))?;
        Ok(Self {
            stream,
            write_cipher,
            read_cipher,
            write_counter: [0; 16],
            read_counter: [0; 16],
            max_record_bytes,
            read_header: [0; 2],
            read_header_pos: 0,
            read_body: Vec::new(),
            read_body_pos: 0,
            read_plain: Vec::new(),
            read_plain_pos: 0,
            write_pending: Vec::new(),
            write_pending_pos: 0,
        })
    }

    fn decrypt_ready_record(&mut self) -> io::Result<()> {
        let ciphertext_len = usize::from(u16::from_be_bytes(self.read_header));
        let (ciphertext, tag_bytes) = self.read_body.split_at_mut(ciphertext_len);
        let tag = GenericArray::clone_from_slice(tag_bytes);
        self.read_cipher
            .decrypt_in_place_detached(
                GenericArray::from_slice(&self.read_counter),
                &self.read_header,
                ciphertext,
                &tag,
            )
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "RA2 AES-EAX authentication failed",
                )
            })?;
        increment_counter(&mut self.read_counter);
        self.read_plain.clear();
        self.read_plain.extend_from_slice(ciphertext);
        self.read_plain_pos = 0;
        self.read_header_pos = 0;
        self.read_body.clear();
        self.read_body_pos = 0;
        Ok(())
    }

    fn prepare_write_record(&mut self, plaintext: &[u8]) -> io::Result<usize> {
        let len = plaintext.len().min(self.max_record_bytes);
        let header = (len as u16).to_be_bytes();
        let mut ciphertext = plaintext[..len].to_vec();
        let tag = self
            .write_cipher
            .encrypt_in_place_detached(
                GenericArray::from_slice(&self.write_counter),
                &header,
                &mut ciphertext,
            )
            .map_err(|_| io::Error::other("RA2 AES-EAX encryption failed"))?;
        increment_counter(&mut self.write_counter);

        self.write_pending.clear();
        self.write_pending.reserve(2 + len + RA2_MAC_BYTES);
        self.write_pending.extend_from_slice(&header);
        self.write_pending.extend_from_slice(&ciphertext);
        self.write_pending.extend_from_slice(&tag);
        self.write_pending_pos = 0;
        Ok(len)
    }

    fn poll_drain_write_pending(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>>
    where
        S: AsyncWrite + Unpin,
    {
        while self.write_pending_pos < self.write_pending.len() {
            let written = match Pin::new(&mut self.stream)
                .poll_write(cx, &self.write_pending[self.write_pending_pos..])
            {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Ok(0)) => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "failed to write RA2 record",
                    )));
                }
                Poll::Ready(Ok(written)) => written,
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
            };
            self.write_pending_pos += written;
        }
        self.write_pending.clear();
        self.write_pending_pos = 0;
        Poll::Ready(Ok(()))
    }
}

impl<S> AsyncRead for Ra2Stream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        output: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        loop {
            if this.read_plain_pos < this.read_plain.len() {
                let remaining = &this.read_plain[this.read_plain_pos..];
                let count = remaining.len().min(output.remaining());
                output.put_slice(&remaining[..count]);
                this.read_plain_pos += count;
                if this.read_plain_pos == this.read_plain.len() {
                    this.read_plain.clear();
                    this.read_plain_pos = 0;
                }
                return Poll::Ready(Ok(()));
            }
            if output.remaining() == 0 {
                return Poll::Ready(Ok(()));
            }
            if !this.write_pending.is_empty() {
                match this.poll_drain_write_pending(cx) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                    Poll::Ready(Ok(())) => {}
                }
            }

            if this.read_header_pos < this.read_header.len() {
                let start = this.read_header_pos;
                let mut buffer = ReadBuf::new(&mut this.read_header[start..]);
                match Pin::new(&mut this.stream).poll_read(cx, &mut buffer) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                    Poll::Ready(Ok(())) => {
                        let read = buffer.filled().len();
                        if read == 0 {
                            if this.read_header_pos == 0 {
                                return Poll::Ready(Ok(()));
                            }
                            return Poll::Ready(Err(io::Error::new(
                                io::ErrorKind::UnexpectedEof,
                                "truncated RA2 record header",
                            )));
                        }
                        this.read_header_pos += read;
                        if this.read_header_pos < this.read_header.len() {
                            continue;
                        }
                        let ciphertext_len = usize::from(u16::from_be_bytes(this.read_header));
                        if ciphertext_len > this.max_record_bytes {
                            return Poll::Ready(Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                format!(
                                    "RA2 record exceeds configured limit: {ciphertext_len} > {}",
                                    this.max_record_bytes
                                ),
                            )));
                        }
                        this.read_body.resize(ciphertext_len + RA2_MAC_BYTES, 0);
                        this.read_body_pos = 0;
                    }
                }
            }

            if this.read_body_pos < this.read_body.len() {
                let start = this.read_body_pos;
                let mut buffer = ReadBuf::new(&mut this.read_body[start..]);
                match Pin::new(&mut this.stream).poll_read(cx, &mut buffer) {
                    Poll::Pending => return Poll::Pending,
                    Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                    Poll::Ready(Ok(())) => {
                        let read = buffer.filled().len();
                        if read == 0 {
                            return Poll::Ready(Err(io::Error::new(
                                io::ErrorKind::UnexpectedEof,
                                "truncated RA2 encrypted record",
                            )));
                        }
                        this.read_body_pos += read;
                        if this.read_body_pos < this.read_body.len() {
                            continue;
                        }
                    }
                }
            }

            if let Err(error) = this.decrypt_ready_record() {
                return Poll::Ready(Err(error));
            }
        }
    }
}

impl<S> AsyncWrite for Ra2Stream<S>
where
    S: AsyncWrite + Unpin,
{
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        input: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if !this.write_pending.is_empty() {
            match this.poll_drain_write_pending(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                Poll::Ready(Ok(())) => {}
            }
        }
        if input.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let consumed = match this.prepare_write_record(input) {
            Ok(consumed) => consumed,
            Err(error) => return Poll::Ready(Err(error)),
        };
        let written = match Pin::new(&mut this.stream)
            .poll_write(cx, &this.write_pending[this.write_pending_pos..])
        {
            Poll::Pending => return Poll::Ready(Ok(consumed)),
            Poll::Ready(Ok(0)) => {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "failed to write RA2 record",
                )));
            }
            Poll::Ready(Ok(written)) => written,
            Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
        };
        this.write_pending_pos += written;
        if this.write_pending_pos == this.write_pending.len() {
            this.write_pending.clear();
            this.write_pending_pos = 0;
        }
        Poll::Ready(Ok(consumed))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if !this.write_pending.is_empty() {
            match this.poll_drain_write_pending(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                Poll::Ready(Ok(())) => {}
            }
        }
        Pin::new(&mut this.stream).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if !this.write_pending.is_empty() {
            match this.poll_drain_write_pending(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                Poll::Ready(Ok(())) => {}
            }
        }
        Pin::new(&mut this.stream).poll_shutdown(cx)
    }
}

fn increment_counter(counter: &mut [u8; 16]) {
    for byte in counter {
        let (next, overflow) = byte.overflowing_add(1);
        *byte = next;
        if !overflow {
            break;
        }
    }
}

#[cfg(test)]
pub(crate) async fn establish_test_encryption<S>(
    stream: S,
    limits: VncLimits,
    corrupt_server_hash: bool,
) -> Ra2Stream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    establish_test_encryption_with_params(
        stream,
        limits,
        corrupt_server_hash,
        CLIENT_RSA_BITS,
        RA2_RANDOM_BYTES,
    )
    .await
}

#[cfg(test)]
async fn establish_test_encryption_with_params<S>(
    mut stream: S,
    limits: VncLimits,
    corrupt_server_hash: bool,
    key_bits: usize,
    random_bytes: usize,
) -> Ra2Stream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut rng = OsRng;
    let server_private_key = RsaPrivateKey::new(&mut rng, key_bits).unwrap();
    let server_public_key = RsaPublicKey::from(&server_private_key);
    let server_wire = encode_public_key(&server_public_key).unwrap();
    stream.write_all(&server_wire).await.unwrap();

    let (client_public_key, client_wire) = read_public_key(&mut stream, &limits).await.unwrap();
    let server_random = vec![0x31_u8; random_bytes];
    let encrypted_server_random = client_public_key
        .encrypt(&mut rng, Pkcs1v15Encrypt, &server_random)
        .unwrap();
    stream
        .write_u16(encrypted_server_random.len() as u16)
        .await
        .unwrap();
    stream.write_all(&encrypted_server_random).await.unwrap();

    let encrypted_client_random_len = usize::from(stream.read_u16().await.unwrap());
    let mut encrypted_client_random = vec![0_u8; encrypted_client_random_len];
    stream
        .read_exact(&mut encrypted_client_random)
        .await
        .unwrap();
    let client_random = server_private_key
        .decrypt(Pkcs1v15Encrypt, &encrypted_client_random)
        .unwrap();
    assert_eq!(client_random.len(), random_bytes);

    let client_session_key = sha256_concat(&server_random, &client_random);
    let server_session_key = sha256_concat(&client_random, &server_random);
    let mut server_hash = sha256_concat(&server_wire, &client_wire);
    let expected_client_hash = sha256_concat(&client_wire, &server_wire);
    let mut encrypted = Ra2Stream::new(
        stream,
        server_session_key,
        client_session_key,
        limits.max_ra2_record_bytes,
    )
    .unwrap();

    if corrupt_server_hash {
        server_hash[0] ^= 0x80;
    }
    encrypted.write_all(&server_hash).await.unwrap();
    if corrupt_server_hash {
        return encrypted;
    }
    let mut client_hash = [0_u8; RA2_HASH_BYTES];
    encrypted.read_exact(&mut client_hash).await.unwrap();
    assert_eq!(client_hash, expected_client_hash);
    encrypted
}

#[cfg(test)]
pub(crate) async fn accept_test_handshake<S>(
    stream: S,
    subtype: u8,
    expected_username: &str,
    expected_password: &str,
    limits: VncLimits,
) -> Ra2Stream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut encrypted = establish_test_encryption(stream, limits, false).await;

    encrypted.write_u8(subtype).await.unwrap();
    let username_len = usize::from(encrypted.read_u8().await.unwrap());
    let mut username = vec![0_u8; username_len];
    encrypted.read_exact(&mut username).await.unwrap();
    let password_len = usize::from(encrypted.read_u8().await.unwrap());
    let mut password = vec![0_u8; password_len];
    encrypted.read_exact(&mut password).await.unwrap();
    assert_eq!(username, expected_username.as_bytes());
    assert_eq!(password, expected_password.as_bytes());
    encrypted.write_u32(0).await.unwrap();
    encrypted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct PartialThenPendingWriter {
        bytes: Vec<u8>,
        write_polls: usize,
    }

    impl AsyncWrite for PartialThenPendingWriter {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            input: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.write_polls += 1;
            match self.write_polls {
                1 => {
                    let written = input.len().min(3);
                    self.bytes.extend_from_slice(&input[..written]);
                    Poll::Ready(Ok(written))
                }
                2 => {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
                _ => {
                    self.bytes.extend_from_slice(input);
                    Poll::Ready(Ok(input.len()))
                }
            }
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    fn paired_streams<S1, S2>(
        left: S1,
        right: S2,
        max_record_bytes: usize,
    ) -> (Ra2Stream<S1>, Ra2Stream<S2>) {
        let left_to_right = [0x11; 32];
        let right_to_left = [0x22; 32];
        (
            Ra2Stream::new(left, left_to_right, right_to_left, max_record_bytes).unwrap(),
            Ra2Stream::new(right, right_to_left, left_to_right, max_record_bytes).unwrap(),
        )
    }

    #[test]
    fn credentials_use_utf8_byte_limits() {
        assert!(validate_credentials("用户", "密码").is_ok());
        let error = validate_credentials(&"é".repeat(128), "password").unwrap_err();
        assert!(matches!(
            error,
            VncError::CredentialTooLong {
                field: "RA2 username",
                actual: 256,
                limit: 255
            }
        ));
    }

    #[test]
    fn encrypted_stream_backpressure_does_not_consume_replacement_input() {
        let writer = PartialThenPendingWriter::default();
        let mut stream = Ra2Stream::new(writer, [0x55; 32], [0x66; 32], 64).unwrap();
        let waker = futures::task::noop_waker_ref();
        let mut cx = Context::from_waker(waker);

        assert!(matches!(
            Pin::new(&mut stream).poll_write(&mut cx, b"first"),
            Poll::Ready(Ok(5))
        ));
        assert!(matches!(
            Pin::new(&mut stream).poll_write(&mut cx, b"cancelled"),
            Poll::Pending
        ));
        assert!(matches!(
            Pin::new(&mut stream).poll_write(&mut cx, b"x"),
            Poll::Ready(Ok(1))
        ));
        assert!(matches!(
            Pin::new(&mut stream).poll_flush(&mut cx),
            Poll::Ready(Ok(()))
        ));

        let first_record_len = 2 + 5 + RA2_MAC_BYTES;
        let bytes = &stream.stream.bytes;
        assert_eq!(bytes.len(), first_record_len + 2 + 1 + RA2_MAC_BYTES);
        assert_eq!(&bytes[..2], &5_u16.to_be_bytes());
        assert_eq!(
            &bytes[first_record_len..first_record_len + 2],
            &1_u16.to_be_bytes()
        );
    }

    #[tokio::test]
    async fn rejects_out_of_range_server_rsa_key_before_allocation() {
        let mut input = &9_000_u32.to_be_bytes()[..];
        let error = read_public_key(&mut input, &VncLimits::default())
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            VncError::InvalidRa2KeyLength {
                actual: 9_000,
                min: 1_024,
                max: 8_192
            }
        ));
    }

    #[tokio::test]
    async fn configured_8192_bit_public_key_limit_is_honored() {
        // Public-key parsing needs a modulus and exponent, not private primes.
        // RsaPublicKey::new defaults to 4096 bits; RA2 allows up to 8192.
        let mut wire = 8192_u32.to_be_bytes().to_vec();
        wire.extend_from_slice(&[0xff; 1024]);
        wire.resize(4 + 2048 - 3, 0);
        wire.extend_from_slice(&[1, 0, 1]);
        let (key, encoded) = read_public_key(&mut wire.as_slice(), &VncLimits::default())
            .await
            .unwrap();
        assert_eq!(key.n().bits(), 8192);
        assert_eq!(encoded, wire);
    }

    #[tokio::test]
    async fn rejects_rsa_modulus_smaller_than_advertised_policy() {
        let key = RsaPrivateKey::new(&mut OsRng, 512).unwrap();
        let mut wire = 1024_u32.to_be_bytes().to_vec();
        wire.resize(4 + 128 - key.n().to_bytes_be().len(), 0);
        wire.extend(key.n().to_bytes_be());
        wire.resize(4 + 256 - key.e().to_bytes_be().len(), 0);
        wire.extend(key.e().to_bytes_be());
        let error = read_public_key(&mut wire.as_slice(), &VncLimits::default())
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            VncError::InvalidRa2KeyLength { actual: 512, .. }
        ));
    }

    #[tokio::test]
    async fn supports_1024_bit_keys_and_tigervnc_256_bit_randoms() {
        let (client, server) = tokio::io::duplex(16 * 1024);
        let limits = VncLimits::default();
        let server_task = tokio::spawn(async move {
            let mut encrypted =
                establish_test_encryption_with_params(server, limits, false, 1024, 32).await;
            encrypted.write_u8(2).await.unwrap();
            assert_eq!(encrypted.read_u8().await.unwrap(), 0);
            assert_eq!(encrypted.read_u8().await.unwrap(), 9);
            let mut password = [0; 9];
            encrypted.read_exact(&mut password).await.unwrap();
            assert_eq!(&password, b"raspberry");
            encrypted.write_u32(0).await.unwrap();
            encrypted.flush().await.unwrap();
        });
        let verifier: ServerKeyVerifier = Arc::new(|key| {
            Box::pin(async move {
                assert_eq!(key.bits(), 1024);
                Ok(())
            })
        });
        assert!(authenticate(
            client,
            String::new(),
            "raspberry".into(),
            verifier,
            limits,
            VncVersion::RFB38
        )
        .await
        .is_ok());
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn rejects_truncated_records_and_oversized_record_headers() {
        for bytes in [vec![0], vec![0, 3, 1, 2], vec![0, 65]] {
            let (mut writer, input) = tokio::io::duplex(128);
            writer.write_all(&bytes).await.unwrap();
            drop(writer);
            let mut reader = Ra2Stream::new(input, [1; 32], [2; 32], 64).unwrap();
            let mut output = [0; 3];
            let error = reader.read_exact(&mut output).await.unwrap_err();
            assert!(matches!(
                error.kind(),
                io::ErrorKind::UnexpectedEof | io::ErrorKind::InvalidData
            ));
        }
    }

    #[test]
    fn message_counter_is_little_endian_and_carries() {
        let mut counter = [0; 16];
        counter[0] = 255;
        increment_counter(&mut counter);
        assert_eq!(&counter[..3], &[0, 1, 0]);
    }

    #[tokio::test]
    async fn authenticate_rejects_server_hash_mismatch() {
        let (client, server) = tokio::io::duplex(16 * 1024);
        let limits = VncLimits::default();
        let server_task = tokio::spawn(async move {
            let _encrypted = establish_test_encryption(server, limits, true).await;
        });
        let verifier: ServerKeyVerifier = Arc::new(|_| Box::pin(async { Ok(()) }));

        let error = match authenticate(
            client,
            "pi".to_string(),
            "raspberry".to_string(),
            verifier,
            limits,
            VncVersion::RFB38,
        )
        .await
        {
            Ok(_) => panic!("corrupted server key hash must fail closed"),
            Err(error) => error,
        };
        assert!(matches!(error, VncError::Ra2ServerHashMismatch));
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn authenticate_rejects_unknown_subtype() {
        let (client, server) = tokio::io::duplex(16 * 1024);
        let limits = VncLimits::default();
        let server_task = tokio::spawn(async move {
            let mut encrypted = establish_test_encryption(server, limits, false).await;
            encrypted.write_u8(3).await.unwrap();
        });
        let verifier: ServerKeyVerifier = Arc::new(|_| Box::pin(async { Ok(()) }));

        let error = match authenticate(
            client,
            "pi".to_string(),
            "raspberry".to_string(),
            verifier,
            limits,
            VncVersion::RFB38,
        )
        .await
        {
            Ok(_) => panic!("unknown RA2 subtype must fail closed"),
            Err(error) => error,
        };
        assert!(matches!(error, VncError::InvalidRa2Subtype(3)));
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn encrypted_stream_splits_and_coalesces_independently_of_rfb_reads() {
        let (left_io, right_io) = tokio::io::duplex(4096);
        let (mut left, mut right) = paired_streams(left_io, right_io, 5);

        let sender = tokio::spawn(async move {
            left.write_all(b"abcdefgh").await.unwrap();
            left.write_all(b"ijklmnop").await.unwrap();
            left
        });

        let mut first = [0_u8; 3];
        right.read_exact(&mut first).await.unwrap();
        assert_eq!(&first, b"abc");
        let mut rest = [0_u8; 13];
        right.read_exact(&mut rest).await.unwrap();
        assert_eq!(&rest, b"defghijklmnop");

        let mut left = sender.await.unwrap();
        right.write_all(b"reply-one").await.unwrap();
        right.write_all(b"reply-two").await.unwrap();
        let mut reply = [0_u8; 18];
        left.read_exact(&mut reply).await.unwrap();
        assert_eq!(&reply, b"reply-onereply-two");
    }

    #[tokio::test]
    async fn encrypted_stream_rejects_bad_mac() {
        let (mut raw_writer, raw_reader) = tokio::io::duplex(128);
        let key = [0x44; 32];
        let mut reader = Ra2Stream::new(raw_reader, [0x33; 32], key, 64).unwrap();
        let cipher = Aes256Eax::new_from_slice(&key).unwrap();
        let header = 3_u16.to_be_bytes();
        let mut ciphertext = b"bad".to_vec();
        let tag = cipher
            .encrypt_in_place_detached(GenericArray::from_slice(&[0; 16]), &header, &mut ciphertext)
            .unwrap();
        raw_writer.write_all(&header).await.unwrap();
        raw_writer.write_all(&ciphertext).await.unwrap();
        let mut bad_tag = tag.to_vec();
        bad_tag[0] ^= 0x80;
        raw_writer.write_all(&bad_tag).await.unwrap();

        let mut output = [0_u8; 3];
        let error = reader.read_exact(&mut output).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }
}
