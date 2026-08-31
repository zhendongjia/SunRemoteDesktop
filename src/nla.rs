use std::io;
use std::path::Path;
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};

use anyhow::{Context, Result, bail, ensure};
use sha2::{Digest, Sha256};
use sspi::CredentialsBuffers;
use sspi::credssp::{NStatusCode, TsRequest, read_ts_credentials};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;
use zeroize::Zeroizing;

use crate::access::AccessGate;

const TPKT_HEADER_SIZE: usize = 4;
const X224_REQUEST_FIXED_SIZE: usize = 11;
const X224_CONFIRM_SIZE: usize = 19;
const MAX_TPKT_SIZE: usize = u16::MAX as usize;
const MAX_TS_REQUEST_SIZE: usize = 1024 * 1024;

const PROTOCOL_SSL: u32 = 0x0000_0001;
const PROTOCOL_HYBRID: u32 = 0x0000_0002;
const PROTOCOL_HYBRID_EX: u32 = 0x0000_0008;

const EARLY_AUTH_SUCCESS: [u8; 4] = 0_u32.to_le_bytes();
const EARLY_AUTH_ACCESS_DENIED: [u8; 4] = 5_u32.to_le_bytes();

const CLIENT_TO_SERVER_BINDING_HASH: &[u8] = b"CredSSP Client-To-Server Binding Hash\0";
const SERVER_TO_CLIENT_BINDING_HASH: &[u8] = b"CredSSP Server-To-Client Binding Hash\0";

pub(crate) enum PreparedConnection {
    Standard(PrefixedStream<TcpStream>),
    Nla(Box<PrefixedStream<TlsStream<TcpStream>>>),
}

/// Performs the part of RDP negotiation that must happen before IronRDP.
///
/// IronRDP's current server-side CredSSP helper accepts only one password
/// preloaded into the process. SunRDP instead uses the Windows Negotiate SSP,
/// so every local/domain account is checked by Windows and no credential is
/// retained by the service. After NLA succeeds, the already-encrypted stream
/// is replayed into IronRDP as a TLS connection.
pub(crate) async fn prepare_connection(
    mut stream: TcpStream,
    tls_acceptor: TlsAcceptor,
    public_key: &[u8],
    config_path: &Path,
    access_gate: &AccessGate,
) -> Result<PreparedConnection> {
    let request = read_tpkt(&mut stream)
        .await
        .context("read initial RDP negotiation request")?;
    let negotiation = parse_negotiation_request(&request)?;

    let Some(selected_protocol) = negotiation.preferred_nla_protocol() else {
        tracing::debug!(
            requested_protocols = format_args!("{:#x}", negotiation.requested_protocols),
            "RDP client did not request NLA; using the encrypted SunRDP access screen"
        );
        return Ok(PreparedConnection::Standard(PrefixedStream::new(
            stream, request, 0,
        )));
    };

    stream
        .write_all(&connection_confirm(selected_protocol))
        .await
        .context("send RDP NLA negotiation response")?;
    let mut stream = tls_acceptor
        .accept(stream)
        .await
        .context("accept TLS before CredSSP")?;

    let authentication =
        authenticate_credssp(&mut stream, public_key, config_path, access_gate).await;
    if let Err(error) = authentication {
        if selected_protocol == PROTOCOL_HYBRID_EX {
            let _ = stream.write_all(&EARLY_AUTH_ACCESS_DENIED).await;
        }
        return Err(error.context("CredSSP authentication failed"));
    }

    if selected_protocol == PROTOCOL_HYBRID_EX {
        stream
            .write_all(&EARLY_AUTH_SUCCESS)
            .await
            .context("send CredSSP early-auth success")?;
    }

    let continuation = tls_continuation_request(request, negotiation.protocol_offset)?;
    tracing::info!("RDP client authenticated with Windows NLA/CredSSP");
    Ok(PreparedConnection::Nla(Box::new(PrefixedStream::new(
        stream,
        continuation,
        X224_CONFIRM_SIZE,
    ))))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct NegotiationRequest {
    requested_protocols: u32,
    protocol_offset: usize,
}

impl NegotiationRequest {
    fn preferred_nla_protocol(self) -> Option<u32> {
        if self.requested_protocols & PROTOCOL_HYBRID_EX != 0 {
            Some(PROTOCOL_HYBRID_EX)
        } else if self.requested_protocols & PROTOCOL_HYBRID != 0 {
            Some(PROTOCOL_HYBRID)
        } else {
            None
        }
    }
}

fn parse_negotiation_request(packet: &[u8]) -> Result<NegotiationRequest> {
    ensure!(
        packet.len() >= X224_REQUEST_FIXED_SIZE + 8,
        "truncated RDP negotiation request"
    );
    ensure!(packet[0] == 3 && packet[1] == 0, "invalid TPKT header");
    ensure!(
        usize::from(u16::from_be_bytes([packet[2], packet[3]])) == packet.len(),
        "incorrect TPKT length"
    );
    ensure!(packet[5] == 0xe0, "expected an X.224 connection request");

    let mut offset = X224_REQUEST_FIXED_SIZE;
    if packet[offset..].starts_with(b"Cookie:") {
        let cookie_end = packet[offset..]
            .windows(2)
            .position(|window| window == b"\r\n")
            .context("RDP cookie is not terminated")?;
        offset += cookie_end + 2;
    }

    ensure!(
        offset + 8 <= packet.len(),
        "RDP negotiation payload is missing"
    );
    ensure!(packet[offset] == 0x01, "expected RDP_NEG_REQ");
    ensure!(
        u16::from_le_bytes([packet[offset + 2], packet[offset + 3]]) == 8,
        "invalid RDP_NEG_REQ length"
    );
    let protocol_offset = offset + 4;
    let requested_protocols = u32::from_le_bytes(
        packet[protocol_offset..protocol_offset + 4]
            .try_into()
            .expect("validated four-byte protocol field"),
    );

    Ok(NegotiationRequest {
        requested_protocols,
        protocol_offset,
    })
}

fn connection_confirm(selected_protocol: u32) -> [u8; X224_CONFIRM_SIZE] {
    let mut response = [
        0x03, 0x00, 0x00, 0x13, // TPKT
        0x0e, 0xd0, 0x00, 0x00, 0x00, 0x00, 0x00, // X.224 confirm
        0x02, 0x00, 0x08, 0x00, // RDP_NEG_RSP
        0x00, 0x00, 0x00, 0x00,
    ];
    response[15..19].copy_from_slice(&selected_protocol.to_le_bytes());
    response
}

fn tls_continuation_request(mut request: Vec<u8>, protocol_offset: usize) -> Result<Vec<u8>> {
    ensure!(
        protocol_offset + 4 <= request.len(),
        "invalid RDP protocol offset"
    );
    request[protocol_offset..protocol_offset + 4].copy_from_slice(&PROTOCOL_SSL.to_le_bytes());
    Ok(request)
}

async fn read_tpkt(stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut header = [0_u8; TPKT_HEADER_SIZE];
    stream.read_exact(&mut header).await?;
    ensure!(header[0] == 3 && header[1] == 0, "invalid TPKT header");
    let length = usize::from(u16::from_be_bytes([header[2], header[3]]));
    ensure!(
        (X224_REQUEST_FIXED_SIZE..=MAX_TPKT_SIZE).contains(&length),
        "invalid TPKT packet length {length}"
    );
    let mut packet = vec![0_u8; length];
    packet[..TPKT_HEADER_SIZE].copy_from_slice(&header);
    stream.read_exact(&mut packet[TPKT_HEADER_SIZE..]).await?;
    Ok(packet)
}

async fn read_ts_request<S>(stream: &mut S) -> Result<TsRequest>
where
    S: AsyncRead + Unpin,
{
    let mut header = [0_u8; 2];
    stream.read_exact(&mut header).await?;
    ensure!(
        header[0] == 0x30,
        "CredSSP message is not an ASN.1 sequence"
    );

    let (header_length, body_length, extra_length_bytes) = if header[1] & 0x80 == 0 {
        (2_usize, usize::from(header[1]), Vec::new())
    } else {
        let length_bytes = usize::from(header[1] & 0x7f);
        ensure!(
            (1..=4).contains(&length_bytes),
            "invalid CredSSP ASN.1 length"
        );
        let mut encoded_length = vec![0_u8; length_bytes];
        stream.read_exact(&mut encoded_length).await?;
        ensure!(encoded_length[0] != 0, "non-canonical CredSSP ASN.1 length");
        let body_length = encoded_length
            .iter()
            .fold(0_usize, |length, byte| (length << 8) | usize::from(*byte));
        (2 + length_bytes, body_length, encoded_length)
    };
    let total_length = header_length
        .checked_add(body_length)
        .context("CredSSP message length overflow")?;
    ensure!(
        total_length <= MAX_TS_REQUEST_SIZE,
        "CredSSP message is too large"
    );

    let mut encoded = Vec::with_capacity(total_length);
    encoded.extend_from_slice(&header);
    encoded.extend_from_slice(&extra_length_bytes);
    encoded.resize(total_length, 0);
    stream.read_exact(&mut encoded[header_length..]).await?;
    TsRequest::from_buffer(&encoded).context("decode CredSSP TSRequest")
}

async fn write_ts_request<S>(stream: &mut S, request: &TsRequest) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    let mut encoded = Vec::with_capacity(usize::from(request.buffer_len()));
    request
        .encode_ts_request(&mut encoded)
        .context("encode CredSSP TSRequest")?;
    ensure!(
        encoded.len() <= MAX_TS_REQUEST_SIZE,
        "CredSSP response is too large"
    );
    stream.write_all(&encoded).await?;
    Ok(())
}

#[cfg(windows)]
async fn authenticate_credssp<S>(
    stream: &mut S,
    public_key: &[u8],
    config_path: &Path,
    access_gate: &AccessGate,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let result = authenticate_credssp_inner(stream, public_key, config_path, access_gate).await;
    if result.is_err() {
        let response = TsRequest {
            version: 6,
            error_code: Some(NStatusCode::LOGON_FAILURE),
            ..TsRequest::default()
        };
        let _ = write_ts_request(stream, &response).await;
    }
    result
}

#[cfg(not(windows))]
async fn authenticate_credssp<S>(
    _stream: &mut S,
    _public_key: &[u8],
    _config_path: &Path,
    _access_gate: &AccessGate,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    bail!("Windows SSPI is required for NLA")
}

#[cfg(windows)]
async fn authenticate_credssp_inner<S>(
    stream: &mut S,
    public_key: &[u8],
    config_path: &Path,
    access_gate: &AccessGate,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut security = NativeNegotiate::acquire()?;
    let mut request = read_ts_request(stream).await?;
    ensure!(
        request.version >= 5,
        "CredSSP versions older than 5 are not accepted"
    );
    let peer_version = request.version.min(6);

    loop {
        ensure!(
            request.version == peer_version || request.version.min(6) == peer_version,
            "CredSSP peer changed protocol version"
        );
        let token = request
            .nego_tokens
            .take()
            .context("CredSSP negotiation token is missing")?;
        let accepted = security.accept(&token)?;

        if !accepted.complete {
            write_ts_request(
                stream,
                &TsRequest {
                    version: peer_version,
                    nego_tokens: Some(accepted.output),
                    ..TsRequest::default()
                },
            )
            .await?;
            request = read_ts_request(stream).await?;
            continue;
        }

        if request.pub_key_auth.is_none() {
            ensure!(
                !accepted.output.is_empty(),
                "CredSSP security package completed without a final token"
            );
            write_ts_request(
                stream,
                &TsRequest {
                    version: peer_version,
                    nego_tokens: Some(accepted.output),
                    ..TsRequest::default()
                },
            )
            .await?;
            request = read_ts_request(stream).await?;
        }
        break;
    }

    let nonce = request
        .client_nonce
        .context("CredSSP client nonce is missing")?;
    let encrypted_client_binding = request
        .pub_key_auth
        .take()
        .context("CredSSP public-key binding is missing")?;
    let client_binding = security.decrypt(&encrypted_client_binding)?;
    let expected_client_binding = binding_hash(CLIENT_TO_SERVER_BINDING_HASH, &nonce, public_key);
    ensure!(
        client_binding == expected_client_binding,
        "CredSSP TLS public-key binding did not match"
    );

    let server_binding = binding_hash(SERVER_TO_CLIENT_BINDING_HASH, &nonce, public_key);
    let encrypted_server_binding = security.encrypt(&server_binding)?;
    write_ts_request(
        stream,
        &TsRequest {
            version: peer_version,
            pub_key_auth: Some(encrypted_server_binding),
            ..TsRequest::default()
        },
    )
    .await?;

    let mut auth_request = read_ts_request(stream).await?;
    ensure!(
        auth_request.version.min(6) == peer_version,
        "CredSSP peer changed protocol version"
    );
    let encrypted_credentials = auth_request
        .auth_info
        .take()
        .context("CredSSP delegated credentials are missing")?;
    let credentials = Zeroizing::new(security.decrypt(&encrypted_credentials)?);
    ensure!(
        matches!(
            read_ts_credentials(credentials.as_slice())?,
            CredentialsBuffers::AuthIdentity(_)
        ),
        "CredSSP delegated an unsupported credential type"
    );

    let account = security.context_name()?;
    let allowed = crate::auth::is_account_allowed(config_path, &account)?;
    if !allowed {
        tracing::warn!(user = %account, "NLA-authenticated account rejected by the SunRDP allow-list");
    }
    ensure!(
        allowed,
        "authenticated Windows account is not on the SunRDP allow-list"
    );

    let generation = access_gate.begin_validation(&account);
    access_gate.finish_validation(generation, &account, Ok(true));
    Ok(())
}

fn binding_hash(magic: &[u8], nonce: &[u8; 32], public_key: &[u8]) -> Vec<u8> {
    let mut digest = Sha256::new();
    digest.update(magic);
    digest.update(nonce);
    digest.update(public_key);
    digest.finalize().to_vec()
}

#[cfg(windows)]
struct AcceptResult {
    output: Vec<u8>,
    complete: bool,
}

#[cfg(windows)]
struct NativeNegotiate {
    credentials: windows::Win32::Security::Credentials::SecHandle,
    context: Option<windows::Win32::Security::Credentials::SecHandle>,
    sizes: Option<windows::Win32::Security::Authentication::Identity::SecPkgContext_Sizes>,
    send_sequence: u32,
    receive_sequence: u32,
}

#[cfg(windows)]
impl NativeNegotiate {
    fn acquire() -> Result<Self> {
        use windows::Win32::Security::Authentication::Identity::{
            AcquireCredentialsHandleW, SECPKG_CRED_INBOUND,
        };
        use windows::Win32::Security::Credentials::SecHandle;

        let mut credentials = SecHandle::default();
        unsafe {
            AcquireCredentialsHandleW(
                None,
                windows::core::w!("Negotiate"),
                SECPKG_CRED_INBOUND,
                None,
                None,
                None,
                None,
                &mut credentials,
                None,
            )
        }
        .context("acquire Windows Negotiate credentials")?;
        Ok(Self {
            credentials,
            context: None,
            sizes: None,
            send_sequence: 0,
            receive_sequence: 0,
        })
    }

    fn accept(&mut self, token: &[u8]) -> Result<AcceptResult> {
        use windows::Win32::Foundation::{
            SEC_E_OK, SEC_I_COMPLETE_AND_CONTINUE, SEC_I_COMPLETE_NEEDED, SEC_I_CONTINUE_NEEDED,
        };
        use windows::Win32::Security::Authentication::Identity::{
            ASC_REQ_ALLOCATE_MEMORY, ASC_REQ_CONFIDENTIALITY, ASC_REQ_CONNECTION,
            ASC_REQ_EXTENDED_ERROR, ASC_REQ_INTEGRITY, ASC_REQ_REPLAY_DETECT,
            ASC_REQ_SEQUENCE_DETECT, AcceptSecurityContext, CompleteAuthToken, FreeContextBuffer,
            SECBUFFER_EMPTY, SECBUFFER_TOKEN, SECBUFFER_VERSION, SECURITY_NATIVE_DREP, SecBuffer,
            SecBufferDesc,
        };
        use windows::Win32::Security::Credentials::SecHandle;

        let mut token = token.to_vec();
        let mut input_buffers = [
            SecBuffer {
                cbBuffer: token.len().try_into().context("SSPI token is too large")?,
                BufferType: SECBUFFER_TOKEN,
                pvBuffer: token.as_mut_ptr().cast(),
            },
            SecBuffer {
                cbBuffer: 0,
                BufferType: SECBUFFER_EMPTY,
                pvBuffer: std::ptr::null_mut(),
            },
        ];
        let input = SecBufferDesc {
            ulVersion: SECBUFFER_VERSION,
            cBuffers: input_buffers.len() as u32,
            pBuffers: input_buffers.as_mut_ptr(),
        };
        let mut output_buffer = SecBuffer {
            cbBuffer: 0,
            BufferType: SECBUFFER_TOKEN,
            pvBuffer: std::ptr::null_mut(),
        };
        let mut output = SecBufferDesc {
            ulVersion: SECBUFFER_VERSION,
            cBuffers: 1,
            pBuffers: &mut output_buffer,
        };
        let mut context = SecHandle::default();
        let mut attributes = 0_u32;
        let requirements = ASC_REQ_ALLOCATE_MEMORY
            | ASC_REQ_CONFIDENTIALITY
            | ASC_REQ_CONNECTION
            | ASC_REQ_EXTENDED_ERROR
            | ASC_REQ_INTEGRITY
            | ASC_REQ_REPLAY_DETECT
            | ASC_REQ_SEQUENCE_DETECT;
        let status = unsafe {
            AcceptSecurityContext(
                Some(&self.credentials),
                self.context.as_ref().map(|context| context as *const _),
                Some(&input),
                requirements,
                SECURITY_NATIVE_DREP,
                Some(&mut context),
                Some(&mut output),
                &mut attributes,
                None,
            )
        };

        if status == SEC_I_COMPLETE_NEEDED || status == SEC_I_COMPLETE_AND_CONTINUE {
            unsafe { CompleteAuthToken(&context, &output) }
                .context("complete Windows Negotiate token")?;
        }

        let output_token = if output_buffer.pvBuffer.is_null() || output_buffer.cbBuffer == 0 {
            Vec::new()
        } else {
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    output_buffer.pvBuffer.cast::<u8>(),
                    output_buffer.cbBuffer as usize,
                )
            }
            .to_vec();
            unsafe { FreeContextBuffer(output_buffer.pvBuffer) }
                .context("free Windows Negotiate output token")?;
            bytes
        };

        let complete = if status == SEC_E_OK || status == SEC_I_COMPLETE_NEEDED {
            true
        } else if status == SEC_I_CONTINUE_NEEDED || status == SEC_I_COMPLETE_AND_CONTINUE {
            false
        } else {
            bail!(
                "Windows Negotiate rejected the client ({:#010x})",
                status.0 as u32
            );
        };
        self.context = Some(context);
        if complete {
            self.query_sizes()?;
        }
        Ok(AcceptResult {
            output: output_token,
            complete,
        })
    }

    fn query_sizes(&mut self) -> Result<()> {
        use windows::Win32::Security::Authentication::Identity::{
            QueryContextAttributesW, SECPKG_ATTR_SIZES, SecPkgContext_Sizes,
        };
        let context = self.context.as_ref().context("SSPI context is missing")?;
        let mut sizes = SecPkgContext_Sizes::default();
        unsafe {
            QueryContextAttributesW(
                context,
                SECPKG_ATTR_SIZES,
                (&mut sizes as *mut SecPkgContext_Sizes).cast(),
            )
        }
        .context("query Windows Negotiate message sizes")?;
        ensure!(
            sizes.cbSecurityTrailer > 0,
            "Windows Negotiate returned an empty security trailer"
        );
        self.sizes = Some(sizes);
        Ok(())
    }

    fn context_name(&self) -> Result<String> {
        use windows::Win32::Security::Authentication::Identity::{
            FreeContextBuffer, QueryContextAttributesW, SECPKG_ATTR_NAMES, SecPkgContext_NamesW,
        };
        let context = self.context.as_ref().context("SSPI context is missing")?;
        let mut names = SecPkgContext_NamesW::default();
        unsafe {
            QueryContextAttributesW(
                context,
                SECPKG_ATTR_NAMES,
                (&mut names as *mut SecPkgContext_NamesW).cast(),
            )
        }
        .context("query authenticated Windows account")?;
        ensure!(
            !names.sUserName.is_null(),
            "Windows Negotiate returned an empty account name"
        );
        let length = (0..32_768)
            .find(|offset| unsafe { *names.sUserName.add(*offset) } == 0)
            .context("authenticated Windows account name is not terminated")?;
        let account = String::from_utf16_lossy(unsafe {
            std::slice::from_raw_parts(names.sUserName, length)
        });
        unsafe { FreeContextBuffer(names.sUserName.cast()) }
            .context("free authenticated Windows account name")?;
        ensure!(
            !account.trim().is_empty(),
            "authenticated Windows account name is empty"
        );
        Ok(account)
    }

    fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>> {
        use windows::Win32::Foundation::SEC_E_OK;
        use windows::Win32::Security::Authentication::Identity::{
            EncryptMessage, SECBUFFER_DATA, SECBUFFER_TOKEN, SECBUFFER_VERSION, SecBuffer,
            SecBufferDesc,
        };
        let context = self.context.as_ref().context("SSPI context is missing")?;
        let sizes = self.sizes.context("SSPI message sizes are missing")?;
        let mut token = vec![0_u8; sizes.cbSecurityTrailer as usize];
        let mut data = plaintext.to_vec();
        let mut buffers = [
            SecBuffer {
                cbBuffer: token.len() as u32,
                BufferType: SECBUFFER_TOKEN,
                pvBuffer: token.as_mut_ptr().cast(),
            },
            SecBuffer {
                cbBuffer: data
                    .len()
                    .try_into()
                    .context("CredSSP plaintext is too large")?,
                BufferType: SECBUFFER_DATA,
                pvBuffer: data.as_mut_ptr().cast(),
            },
        ];
        let descriptor = SecBufferDesc {
            ulVersion: SECBUFFER_VERSION,
            cBuffers: buffers.len() as u32,
            pBuffers: buffers.as_mut_ptr(),
        };
        let status = unsafe { EncryptMessage(context, 0, &descriptor, self.send_sequence) };
        ensure!(
            status == SEC_E_OK,
            "Windows Negotiate could not encrypt CredSSP data ({:#010x})",
            status.0 as u32
        );
        self.send_sequence = self.send_sequence.wrapping_add(1);
        token.truncate(buffers[0].cbBuffer as usize);
        data.truncate(buffers[1].cbBuffer as usize);
        token.extend_from_slice(&data);
        Ok(token)
    }

    fn decrypt(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>> {
        use windows::Win32::Foundation::SEC_E_OK;
        use windows::Win32::Security::Authentication::Identity::{
            DecryptMessage, SECBUFFER_DATA, SECBUFFER_TOKEN, SECBUFFER_VERSION, SecBuffer,
            SecBufferDesc,
        };
        let context = self.context.as_ref().context("SSPI context is missing")?;
        let sizes = self.sizes.context("SSPI message sizes are missing")?;
        let trailer_length = sizes.cbSecurityTrailer as usize;
        ensure!(
            ciphertext.len() >= trailer_length,
            "truncated CredSSP protected message"
        );
        let mut protected = ciphertext.to_vec();
        let mut buffers = [
            SecBuffer {
                cbBuffer: trailer_length as u32,
                BufferType: SECBUFFER_TOKEN,
                pvBuffer: protected.as_mut_ptr().cast(),
            },
            SecBuffer {
                cbBuffer: (protected.len() - trailer_length)
                    .try_into()
                    .context("CredSSP ciphertext is too large")?,
                BufferType: SECBUFFER_DATA,
                pvBuffer: unsafe { protected.as_mut_ptr().add(trailer_length) }.cast(),
            },
        ];
        let descriptor = SecBufferDesc {
            ulVersion: SECBUFFER_VERSION,
            cBuffers: buffers.len() as u32,
            pBuffers: buffers.as_mut_ptr(),
        };
        let mut quality = 0_u32;
        let status = unsafe {
            DecryptMessage(
                context,
                &descriptor,
                self.receive_sequence,
                Some(&mut quality),
            )
        };
        ensure!(
            status == SEC_E_OK,
            "Windows Negotiate could not decrypt CredSSP data ({:#010x})",
            status.0 as u32
        );
        self.receive_sequence = self.receive_sequence.wrapping_add(1);

        let data = buffers
            .iter()
            .find(|buffer| buffer.BufferType == SECBUFFER_DATA)
            .context("Windows Negotiate returned no decrypted data")?;
        ensure!(
            !data.pvBuffer.is_null() || data.cbBuffer == 0,
            "Windows Negotiate returned an invalid data buffer"
        );
        Ok(unsafe {
            std::slice::from_raw_parts(data.pvBuffer.cast::<u8>(), data.cbBuffer as usize)
        }
        .to_vec())
    }
}

#[cfg(windows)]
impl Drop for NativeNegotiate {
    fn drop(&mut self) {
        use windows::Win32::Security::Authentication::Identity::{
            DeleteSecurityContext, FreeCredentialsHandle,
        };
        if let Some(context) = self.context.as_ref() {
            let _ = unsafe { DeleteSecurityContext(context) };
        }
        let _ = unsafe { FreeCredentialsHandle(&self.credentials) };
    }
}

pub(crate) struct PrefixedStream<S> {
    inner: S,
    prefix: Vec<u8>,
    prefix_offset: usize,
    discard_output: usize,
}

impl<S> PrefixedStream<S> {
    fn new(inner: S, prefix: Vec<u8>, discard_output: usize) -> Self {
        Self {
            inner,
            prefix,
            prefix_offset: 0,
            discard_output,
        }
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for PrefixedStream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.prefix_offset < self.prefix.len() && buffer.remaining() > 0 {
            let remaining = &self.prefix[self.prefix_offset..];
            let count = remaining.len().min(buffer.remaining());
            buffer.put_slice(&remaining[..count]);
            self.prefix_offset += count;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(context, buffer)
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for PrefixedStream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &[u8],
    ) -> Poll<io::Result<usize>> {
        if self.discard_output > 0 {
            let discarded = self.discard_output.min(buffer.len());
            self.discard_output -= discarded;
            if discarded > 0 {
                return Poll::Ready(Ok(discarded));
            }
        }
        Pin::new(&mut self.inner).poll_write(context, buffer)
    }

    fn poll_flush(mut self: Pin<&mut Self>, context: &mut TaskContext<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(context)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request_with_cookie(protocols: u32) -> Vec<u8> {
        let mut packet = vec![
            0x03, 0x00, 0x00, 0x00, 0x2a, 0xe0, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        packet.extend_from_slice(b"Cookie: mstshash=test\r\n");
        packet.extend_from_slice(&[0x01, 0x08, 0x08, 0x00]);
        packet.extend_from_slice(&protocols.to_le_bytes());
        packet.extend_from_slice(&[0x06, 0x00, 0x24, 0x00]);
        packet.extend_from_slice(&[0x11; 16]);
        packet.extend_from_slice(&[0; 16]);
        let length = packet.len() as u16;
        packet[2..4].copy_from_slice(&length.to_be_bytes());
        packet
    }

    #[test]
    fn parses_cookie_and_prefers_hybrid_ex() {
        let request = request_with_cookie(PROTOCOL_SSL | PROTOCOL_HYBRID | PROTOCOL_HYBRID_EX);
        let negotiation = parse_negotiation_request(&request).unwrap();
        assert_eq!(
            negotiation.preferred_nla_protocol(),
            Some(PROTOCOL_HYBRID_EX)
        );
        assert_eq!(negotiation.requested_protocols, 0x0b);
    }

    #[test]
    fn continuation_preserves_request_except_for_protocol() {
        let request = request_with_cookie(PROTOCOL_SSL | PROTOCOL_HYBRID_EX);
        let negotiation = parse_negotiation_request(&request).unwrap();
        let continued =
            tls_continuation_request(request.clone(), negotiation.protocol_offset).unwrap();
        assert_eq!(
            &continued[negotiation.protocol_offset..negotiation.protocol_offset + 4],
            &PROTOCOL_SSL.to_le_bytes()
        );
        let mut expected = request;
        expected[negotiation.protocol_offset..negotiation.protocol_offset + 4]
            .copy_from_slice(&PROTOCOL_SSL.to_le_bytes());
        assert_eq!(continued, expected);
    }

    #[test]
    fn confirm_selects_requested_protocol() {
        let confirm = connection_confirm(PROTOCOL_HYBRID_EX);
        assert_eq!(&confirm[..4], &[3, 0, 0, 19]);
        assert_eq!(&confirm[11..15], &[2, 0, 8, 0]);
        assert_eq!(&confirm[15..19], &PROTOCOL_HYBRID_EX.to_le_bytes());
    }

    #[tokio::test]
    async fn prefixed_stream_replays_input_and_discards_one_confirm() {
        let (inner, mut peer) = tokio::io::duplex(128);
        peer.write_all(b"tail").await.unwrap();
        let mut stream = PrefixedStream::new(inner, b"head".to_vec(), X224_CONFIRM_SIZE);
        let mut input = [0_u8; 8];
        stream.read_exact(&mut input).await.unwrap();
        assert_eq!(&input, b"headtail");

        stream.write_all(&[0x55; X224_CONFIRM_SIZE]).await.unwrap();
        stream.write_all(b"visible").await.unwrap();
        let mut output = [0_u8; 7];
        peer.read_exact(&mut output).await.unwrap();
        assert_eq!(&output, b"visible");
    }
}
