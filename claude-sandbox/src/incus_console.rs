//! Incus VGA console sessions over the REST API.
//!
//! The Incus CLI launches a local SPICE viewer when it finds one, so the bridge
//! speaks the console protocol itself: it creates the console operation, keeps
//! its control WebSocket open for the session's lifetime, and opens one data
//! WebSocket per SPICE channel.
//!
//! Each WebSocket owns one rustls connection that a reader thread and writer
//! threads share. Socket reads happen outside every lock; the TLS state is only
//! locked for in-memory work. Writers take the socket-writer lock and then the
//! TLS lock, so records reach the socket in the order they were produced. The
//! reader never waits for the socket-writer lock: control replies are queued
//! in the TLS state and flushed by whichever thread holds or next takes the
//! writer lock, so a writer blocked on a full socket cannot stop the reader.

use std::collections::HashMap;
use std::fs;
use std::io::{self, BufRead, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpStream, ToSocketAddrs};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering, fence};
use std::sync::{Arc, Mutex, TryLockError, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use base64::Engine;
use rustls::client::Resumption;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{
    SecureRandom, WebPkiSupportedAlgorithms, verify_tls12_signature, verify_tls13_signature,
};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{
    CertificateError, ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme,
};
use serde_json::{Value, json};

use crate::websocket::{self, Opcode};

const DEFAULT_PORT: u16 = 8443;
const HEADER_LIMIT: usize = 64 * 1024;
const BODY_LIMIT: usize = 1024 * 1024;
const CHUNK_SIZE: usize = 16 * 1024;
const DATA_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

struct Address {
    host: String,
    port: u16,
    authority: String,
    server_name: ServerName<'static>,
}

/// The bridge's Incus remote: its HTTPS addresses, in the order the Incus
/// client tries them, and a TLS configuration that trusts only the pinned
/// server certificate.
struct Endpoint {
    addresses: Vec<Address>,
    preferred: AtomicUsize,
    config: Arc<ClientConfig>,
    random: &'static dyn SecureRandom,
}

impl Endpoint {
    fn load(conf_dir: &Path, remote: &str) -> Result<Self, String> {
        let config = fs::read_to_string(conf_dir.join("config.yml"))
            .map_err(|error| format!("could not read the Incus client configuration: {error}"))?;
        let addresses = remote_addresses(&config, remote)?
            .into_iter()
            .map(|(host, port)| {
                let authority = if host.contains(':') {
                    format!("[{host}]:{port}")
                } else {
                    format!("{host}:{port}")
                };
                let server_name = ServerName::try_from(host.clone())
                    .map_err(|_| format!("the {remote} remote has an invalid host '{host}'"))?;
                Ok(Address {
                    host,
                    port,
                    authority,
                    server_name,
                })
            })
            .collect::<Result<Vec<_>, String>>()?;

        let server_cert_path = conf_dir.join("servercerts").join(format!("{remote}.crt"));
        if !server_cert_path.is_file() {
            return Err(format!(
                "the pinned Incus server certificate {} is missing; recreate the remote with `INCUS_CONF=~/.claude-sandbox/incus incus remote add {remote} <token>`",
                server_cert_path.display()
            ));
        }
        let server_cert = CertificateDer::from_pem_file(&server_cert_path).map_err(|error| {
            format!("could not read the pinned Incus server certificate: {error}")
        })?;

        // Same lookup as the Incus client: a remote-specific pair wins over the default one.
        let remote_cert = conf_dir.join("clientcerts").join(format!("{remote}.crt"));
        let remote_key = conf_dir.join("clientcerts").join(format!("{remote}.key"));
        let (cert_path, key_path) = if remote_cert.exists() && remote_key.exists() {
            (remote_cert, remote_key)
        } else {
            (conf_dir.join("client.crt"), conf_dir.join("client.key"))
        };
        let client_chain = CertificateDer::pem_file_iter(&cert_path)
            .and_then(|certs| certs.collect::<Result<Vec<_>, _>>())
            .map_err(|error| format!("could not read the Incus client certificate: {error}"))?;
        let client_key = PrivateKeyDer::from_pem_file(&key_path)
            .map_err(|error| format!("could not read the Incus client key: {error}"))?;

        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let verifier = Arc::new(PinnedServerCert {
            certificate: server_cert,
            algorithms: provider.signature_verification_algorithms,
        });
        let random = provider.secure_random;
        // The Incus client and server default to TLS 1.3 only.
        let mut config = ClientConfig::builder_with_provider(provider)
            .with_protocol_versions(&[&rustls::version::TLS13])
            .map_err(|error| format!("could not configure TLS: {error}"))?
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_client_auth_cert(client_chain, client_key)
            .map_err(|error| format!("could not use the Incus client certificate: {error}"))?;
        config.resumption = Resumption::disabled();
        Ok(Self {
            addresses,
            preferred: AtomicUsize::new(0),
            config: Arc::new(config),
            random,
        })
    }

    /// Connects to the first address that completes a pinned TLS handshake,
    /// starting with the one that worked last.
    fn connect(
        &self,
        deadline: Instant,
    ) -> Result<(ClientConnection, TcpStream, &Address), String> {
        let first = self.preferred.load(Ordering::Relaxed);
        let mut last_error = String::new();
        for index in (0..self.addresses.len()).map(|n| (first + n) % self.addresses.len()) {
            let address = &self.addresses[index];
            match self.connect_to(address, deadline) {
                Ok((conn, socket)) => {
                    self.preferred.store(index, Ordering::Relaxed);
                    return Ok((conn, socket, address));
                }
                Err(error) => last_error = error,
            }
        }
        Err(last_error)
    }

    fn connect_to(
        &self,
        address: &Address,
        deadline: Instant,
    ) -> Result<(ClientConnection, TcpStream), String> {
        let resolved = resolve(&address.host, address.port, deadline)
            .map_err(|error| format!("could not resolve {}: {error}", address.authority))?;
        let mut last_error = None;
        let mut socket = None;
        for target in resolved {
            let remaining = remaining(deadline).map_err(|error| error.to_string())?;
            match TcpStream::connect_timeout(&target, remaining) {
                Ok(stream) => {
                    socket = Some(stream);
                    break;
                }
                Err(error) => last_error = Some(error),
            }
        }
        let socket = socket.ok_or_else(|| match last_error {
            Some(error) => format!(
                "could not connect to Incus at {}: {error}",
                address.authority
            ),
            None => format!("{} has no addresses", address.authority),
        })?;
        let _ = socket.set_nodelay(true);
        let mut conn = ClientConnection::new(Arc::clone(&self.config), address.server_name.clone())
            .map_err(|error| format!("could not start TLS: {error}"))?;
        let mut io = DeadlineIo {
            socket: &socket,
            deadline,
        };
        while conn.is_handshaking() {
            match conn.complete_io(&mut io) {
                Ok((0, 0)) => {
                    return Err("Incus closed the connection during the TLS handshake".to_string());
                }
                Ok(_) => {}
                Err(error) => return Err(format!("TLS handshake with Incus failed: {error}")),
            }
        }
        Ok((conn, socket))
    }

    fn post_json(&self, path: &str, body: &Value, deadline: Instant) -> Result<Value, String> {
        let body = body.to_string();
        let (mut conn, socket, address) = self.connect(deadline)?;
        let mut io = DeadlineIo {
            socket: &socket,
            deadline,
        };
        let mut stream = rustls::Stream::new(&mut conn, &mut io);
        let request = format!(
            "POST {path} HTTP/1.1\r\nHost: {}\r\nUser-Agent: claude-sandbox\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            address.authority,
            body.len()
        );
        stream
            .write_all(request.as_bytes())
            .and_then(|()| stream.flush())
            .map_err(|error| format!("failed sending the Incus request: {error}"))?;
        let head = read_head(&mut stream)?;
        let body = read_body(&mut stream, &head)?;
        let _ = socket.shutdown(Shutdown::Both);
        let value: Value =
            serde_json::from_slice(&body).map_err(|_| incus_error(head.status, &body))?;
        if head.status >= 400 || value.get("type").and_then(Value::as_str) == Some("error") {
            return Err(incus_error(head.status, &body));
        }
        Ok(value)
    }

    fn open_websocket(
        &self,
        path: &str,
        deadline: Instant,
    ) -> Result<(WebSocketReader, Arc<WebSocket>), String> {
        let (mut conn, socket, address) = self.connect(deadline)?;
        let mut key = [0_u8; 16];
        self.random
            .fill(&mut key)
            .map_err(|_| "could not generate a WebSocket key".to_string())?;
        let key = base64::engine::general_purpose::STANDARD.encode(key);
        {
            let mut io = DeadlineIo {
                socket: &socket,
                deadline,
            };
            let mut stream = rustls::Stream::new(&mut conn, &mut io);
            let request = format!(
                "GET {path} HTTP/1.1\r\nHost: {}\r\nUser-Agent: claude-sandbox\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n",
                address.authority
            );
            stream
                .write_all(request.as_bytes())
                .and_then(|()| stream.flush())
                .map_err(|error| format!("failed sending the WebSocket request: {error}"))?;
            // Stream reads only the plaintext it needs, so frames the server sends
            // right after the response stay buffered in the TLS connection.
            let head = read_head(&mut stream)?;
            if head.status != 101 {
                let body = read_body(&mut stream, &head).unwrap_or_default();
                return Err(incus_error(head.status, &body));
            }
            let upgraded = head
                .header("upgrade")
                .is_some_and(|value| value.eq_ignore_ascii_case("websocket"));
            if !upgraded
                || head.header("sec-websocket-accept") != Some(websocket::accept_key(&key).as_str())
            {
                return Err("Incus sent an invalid WebSocket handshake".to_string());
            }
        }
        socket
            .set_read_timeout(None)
            .and_then(|()| socket.set_write_timeout(Some(WRITE_TIMEOUT)))
            .map_err(|error| format!("could not configure the WebSocket: {error}"))?;
        let clone = || {
            socket
                .try_clone()
                .map_err(|error| format!("could not clone the WebSocket: {error}"))
        };
        let websocket = Arc::new(WebSocket {
            conn: Mutex::new(conn),
            socket: Mutex::new(clone()?),
            shutdown: clone()?,
            flush_pending: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            random: self.random,
        });
        let reader = WebSocketReader {
            socket,
            websocket: Arc::clone(&websocket),
            buffer: vec![0; CHUNK_SIZE],
            start: 0,
            end: 0,
        };
        Ok((reader, websocket))
    }
}

/// A live `incus console --type=vga` equivalent. The operation, and with it
/// every data connection, lasts exactly as long as the control WebSocket.
pub(crate) struct ConsoleSession {
    endpoint: Endpoint,
    data_path: String,
    control: Arc<WebSocket>,
    open: Arc<AtomicBool>,
}

impl ConsoleSession {
    pub(crate) fn start(
        conf_dir: &Path,
        remote: &str,
        project: &str,
        instance: &str,
        timeout: Duration,
    ) -> Result<Self, String> {
        let deadline = Instant::now() + timeout;
        let endpoint = Endpoint::load(conf_dir, remote)?;
        // The project belongs to the bridge, so `force` only ever takes over a session an
        // earlier relay left behind. Servers without the console_force extension ignore it.
        let response = endpoint
            .post_json(
                &format!("/1.0/instances/{instance}/console?project={project}"),
                &json!({"type": "vga", "width": 0, "height": 0, "force": true}),
                deadline,
            )
            .map_err(|error| format!("could not start the Incus VGA console: {error}"))?;
        let operation = response
            .pointer("/metadata/id")
            .and_then(Value::as_str)
            .filter(|id| valid_token(id))
            .ok_or_else(|| "Incus returned a console operation without a valid id".to_string())?;
        let secret = |name: &str| {
            response
                .pointer(&format!("/metadata/metadata/fds/{name}"))
                .and_then(Value::as_str)
                .filter(|secret| valid_token(secret))
                .ok_or_else(|| {
                    format!("Incus returned a console operation without a {name} secret")
                })
        };
        let control_path = format!(
            "/1.0/operations/{operation}/websocket?secret={}",
            secret("control")?
        );
        let data_path = format!(
            "/1.0/operations/{operation}/websocket?secret={}",
            secret("0")?
        );

        let (mut reader, control) =
            endpoint
                .open_websocket(&control_path, deadline)
                .map_err(|error| {
                    format!("could not connect the Incus console control channel: {error}")
                })?;
        let open = Arc::new(AtomicBool::new(true));
        let control_open = Arc::clone(&open);
        let control_socket = Arc::clone(&control);
        thread::spawn(move || {
            // The server only ever sends pings and the final close on this channel.
            while let Ok(Some(_)) = reader.next_payload() {}
            control_open.store(false, Ordering::Release);
            control_socket.close();
        });
        Ok(Self {
            endpoint,
            data_path,
            control,
            open,
        })
    }

    pub(crate) fn is_open(&self) -> bool {
        self.open.load(Ordering::Acquire)
    }

    pub(crate) fn connect(&self) -> Result<(WebSocketReader, Arc<WebSocket>), String> {
        self.endpoint
            .open_websocket(&self.data_path, Instant::now() + DATA_CONNECT_TIMEOUT)
    }

    pub(crate) fn close(&self) {
        self.open.store(false, Ordering::Release);
        self.control.close();
    }
}

pub(crate) struct WebSocket {
    conn: Mutex<ClientConnection>,
    socket: Mutex<TcpStream>,
    shutdown: TcpStream,
    flush_pending: AtomicBool,
    closed: AtomicBool,
    random: &'static dyn SecureRandom,
}

impl WebSocket {
    fn frame(&self, opcode: Opcode, payload: &[u8]) -> io::Result<Vec<u8>> {
        let mut mask = [0_u8; 4];
        self.random
            .fill(&mut mask)
            .map_err(|_| io::Error::other("could not generate a WebSocket mask"))?;
        let mut frame = Vec::with_capacity(payload.len() + 14);
        websocket::write_masked_frame(&mut frame, opcode, payload, mask)?;
        Ok(frame)
    }

    /// Sends a frame, waiting while the socket is full.
    fn send(&self, opcode: Opcode, payload: &[u8]) -> io::Result<()> {
        if self.closed.load(Ordering::Acquire) {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        let frame = self.frame(opcode, payload)?;
        let mut socket = self.socket.lock().expect("websocket socket lock poisoned");
        self.conn
            .lock()
            .expect("websocket TLS lock poisoned")
            .writer()
            .write_all(&frame)?;
        let result = self.drain(&mut socket);
        drop(socket);
        result?;
        fence(Ordering::SeqCst);
        if self.flush_pending.load(Ordering::SeqCst) {
            self.flush()?;
        }
        Ok(())
    }

    /// Queues a frame without waiting for another thread's socket write.
    fn queue(&self, opcode: Opcode, payload: &[u8]) -> io::Result<()> {
        let frame = self.frame(opcode, payload)?;
        self.conn
            .lock()
            .expect("websocket TLS lock poisoned")
            .writer()
            .write_all(&frame)?;
        self.flush()
    }

    /// Writes out queued TLS records unless another thread holds the socket, in
    /// which case that thread sees `flush_pending` after releasing it.
    fn flush(&self) -> io::Result<()> {
        loop {
            self.flush_pending.store(true, Ordering::SeqCst);
            fence(Ordering::SeqCst);
            let mut socket = match self.socket.try_lock() {
                Ok(socket) => socket,
                Err(TryLockError::WouldBlock) => return Ok(()),
                Err(TryLockError::Poisoned(_)) => panic!("websocket socket lock poisoned"),
            };
            self.flush_pending.store(false, Ordering::SeqCst);
            let result = self.drain(&mut socket);
            drop(socket);
            fence(Ordering::SeqCst);
            if result.is_err() || !self.flush_pending.load(Ordering::SeqCst) {
                return result;
            }
        }
    }

    fn drain(&self, socket: &mut TcpStream) -> io::Result<()> {
        loop {
            let mut output = Vec::new();
            {
                let mut conn = self.conn.lock().expect("websocket TLS lock poisoned");
                while conn.wants_write() {
                    conn.write_tls(&mut output)?;
                }
            }
            if output.is_empty() {
                return Ok(());
            }
            socket.write_all(&output)?;
        }
    }

    fn close_with(&self, payload: &[u8]) {
        if !self.closed.swap(true, Ordering::AcqRel)
            && let Ok(frame) = self.frame(Opcode::Close, payload)
        {
            {
                let mut conn = self.conn.lock().expect("websocket TLS lock poisoned");
                let _ = conn.writer().write_all(&frame);
                conn.send_close_notify();
            }
            let _ = self.flush();
        }
        let _ = self.shutdown.shutdown(Shutdown::Both);
    }

    pub(crate) fn close(&self) {
        self.close_with(&1000_u16.to_be_bytes());
    }

    /// Disconnects immediately; safe while other threads are blocked on the socket.
    pub(crate) fn shutdown(&self) {
        self.closed.store(true, Ordering::Release);
        let _ = self.shutdown.shutdown(Shutdown::Both);
    }
}

pub(crate) struct WebSocketReader {
    socket: TcpStream,
    websocket: Arc<WebSocket>,
    buffer: Vec<u8>,
    start: usize,
    end: usize,
}

impl WebSocketReader {
    /// Returns the next data payload, answering pings on the way, or `None`
    /// once the server ends the stream.
    fn next_payload(&mut self) -> io::Result<Option<Vec<u8>>> {
        loop {
            let frame = websocket::read_server_frame(self)?;
            match frame.opcode {
                Opcode::Binary | Opcode::Continuation => return Ok(Some(frame.payload)),
                Opcode::Ping => self.websocket.queue(Opcode::Pong, &frame.payload)?,
                Opcode::Pong => {}
                Opcode::Close => {
                    self.websocket
                        .close_with(&frame.payload[..frame.payload.len().min(2)]);
                    return Ok(None);
                }
                // Incus marks the end of a stream with a text message.
                Opcode::Text => {
                    self.websocket.close();
                    return Ok(None);
                }
            }
        }
    }
}

impl Read for WebSocketReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            {
                let mut conn = self
                    .websocket
                    .conn
                    .lock()
                    .expect("websocket TLS lock poisoned");
                match conn.reader().read(buf) {
                    Ok(count) => return Ok(count),
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                    Err(error) => return Err(error),
                }
                if self.start < self.end {
                    let mut input = &self.buffer[self.start..self.end];
                    let consumed = conn.read_tls(&mut input)?;
                    self.start += consumed;
                    let processed = conn.process_new_packets();
                    let wants_write = conn.wants_write();
                    drop(conn);
                    // Alerts and key updates go out without waiting for a writer.
                    if wants_write {
                        self.websocket.flush()?;
                    }
                    processed.map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
                    if consumed == 0 {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "TLS connection stopped accepting input",
                        ));
                    }
                    continue;
                }
            }
            let count = (&self.socket).read(&mut self.buffer)?;
            if count == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            self.start = 0;
            self.end = count;
        }
    }
}

/// Relays one SPICE channel between a local client and a data WebSocket until
/// either side closes.
pub(crate) fn relay(client: UnixStream, mut reader: WebSocketReader, websocket: Arc<WebSocket>) {
    let (Ok(mut client_reader), Ok(client_shutdown)) = (client.try_clone(), client.try_clone())
    else {
        websocket.close();
        let _ = client.shutdown(Shutdown::Both);
        return;
    };
    let upstream = Arc::clone(&websocket);
    let forward = thread::spawn(move || {
        let mut buffer = vec![0_u8; CHUNK_SIZE];
        loop {
            match client_reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    if upstream.send(Opcode::Binary, &buffer[..count]).is_err() {
                        break;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => break,
            }
        }
        upstream.close();
        let _ = client_shutdown.shutdown(Shutdown::Both);
    });
    let mut client_writer = client;
    while let Ok(Some(payload)) = reader.next_payload() {
        if client_writer.write_all(&payload).is_err() {
            break;
        }
    }
    websocket.close();
    let _ = client_writer.shutdown(Shutdown::Both);
    let _ = forward.join();
}

#[derive(Debug)]
struct PinnedServerCert {
    certificate: CertificateDer<'static>,
    algorithms: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for PinnedServerCert {
    // The certificate recorded by `incus remote add` is the server's identity, so
    // the host name and validity period play no part; nothing else is trusted.
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        if end_entity.as_ref() == self.certificate.as_ref() {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::InvalidCertificate(
                CertificateError::ApplicationVerificationFailure,
            ))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// Applies the remaining time before `deadline` to every socket read and write.
struct DeadlineIo<'a> {
    socket: &'a TcpStream,
    deadline: Instant,
}

impl Read for DeadlineIo<'_> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.socket
            .set_read_timeout(Some(remaining(self.deadline)?))?;
        self.socket.read(buf).map_err(timed_out)
    }
}

impl Write for DeadlineIo<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.socket
            .set_write_timeout(Some(remaining(self.deadline)?))?;
        self.socket.write(buf).map_err(timed_out)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// The resolver call itself cannot time out, so it runs on its own thread and
// is abandoned once the deadline passes.
fn resolve(host: &str, port: u16, deadline: Instant) -> io::Result<Vec<SocketAddr>> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Ok(vec![SocketAddr::new(ip, port)]);
    }
    let (sender, receiver) = mpsc::channel();
    let target = (host.to_string(), port);
    thread::spawn(move || {
        let _ = sender.send(target.to_socket_addrs().map(Vec::from_iter));
    });
    receiver
        .recv_timeout(remaining(deadline)?)
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "name resolution timed out"))?
}

fn remaining(deadline: Instant) -> io::Result<Duration> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "Incus did not answer in time",
        ))
    } else {
        Ok(remaining)
    }
}

// Socket timeouts surface as WouldBlock, which rustls treats as "try again".
fn timed_out(error: io::Error) -> io::Error {
    if error.kind() == io::ErrorKind::WouldBlock {
        io::Error::new(io::ErrorKind::TimedOut, "Incus did not answer in time")
    } else {
        error
    }
}

struct ResponseHead {
    status: u16,
    headers: Vec<(String, String)>,
}

impl ResponseHead {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

fn read_head(reader: &mut impl BufRead) -> Result<ResponseHead, String> {
    let mut lines = Vec::new();
    let mut total = 0;
    loop {
        let mut line = Vec::new();
        let count = reader
            .by_ref()
            .take((HEADER_LIMIT - total) as u64)
            .read_until(b'\n', &mut line)
            .map_err(|error| format!("failed reading the Incus response: {error}"))?;
        total += count;
        if !line.ends_with(b"\n") {
            return Err("Incus sent an incomplete or oversized response header".to_string());
        }
        let line = String::from_utf8_lossy(&line)
            .trim_end_matches(['\r', '\n'])
            .to_string();
        if line.is_empty() {
            break;
        }
        lines.push(line);
    }
    let status = lines
        .first()
        .filter(|line| line.starts_with("HTTP/1."))
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|status| status.parse().ok())
        .ok_or_else(|| "Incus sent an invalid HTTP status line".to_string())?;
    let headers = lines[1..]
        .iter()
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_string()))
        .collect();
    Ok(ResponseHead { status, headers })
}

fn read_body(reader: &mut impl BufRead, head: &ResponseHead) -> Result<Vec<u8>, String> {
    let failed = |error: io::Error| format!("failed reading the Incus response: {error}");
    let too_large = || format!("Incus response exceeds {BODY_LIMIT} bytes");
    let mut body = Vec::new();
    if head
        .header("transfer-encoding")
        .is_some_and(|value| value.to_ascii_lowercase().contains("chunked"))
    {
        loop {
            let mut line = String::new();
            reader
                .by_ref()
                .take(1024)
                .read_line(&mut line)
                .map_err(failed)?;
            let size = line.split(';').next().unwrap_or("").trim();
            let size = usize::from_str_radix(size, 16)
                .map_err(|_| "Incus sent an invalid chunked response".to_string())?;
            if size == 0 {
                loop {
                    line.clear();
                    reader
                        .by_ref()
                        .take(1024)
                        .read_line(&mut line)
                        .map_err(failed)?;
                    if line.trim().is_empty() {
                        return Ok(body);
                    }
                }
            }
            if size > BODY_LIMIT - body.len() {
                return Err(too_large());
            }
            let start = body.len();
            body.resize(start + size, 0);
            reader.read_exact(&mut body[start..]).map_err(failed)?;
            let mut crlf = [0_u8; 2];
            reader.read_exact(&mut crlf).map_err(failed)?;
            if crlf != *b"\r\n" {
                return Err("Incus sent an invalid chunked response".to_string());
            }
        }
    }
    if let Some(length) = head.header("content-length") {
        let length: usize = length
            .parse()
            .map_err(|_| "Incus sent an invalid Content-Length".to_string())?;
        if length > BODY_LIMIT {
            return Err(too_large());
        }
        body.resize(length, 0);
        reader.read_exact(&mut body).map_err(failed)?;
        return Ok(body);
    }
    match reader
        .by_ref()
        .take(BODY_LIMIT as u64 + 1)
        .read_to_end(&mut body)
    {
        // Servers may skip close_notify on a response delimited by the connection close.
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {}
        Err(error) => return Err(failed(error)),
    }
    if body.len() > BODY_LIMIT {
        return Err(too_large());
    }
    Ok(body)
}

fn incus_error(status: u16, body: &[u8]) -> String {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(Value::as_str)
                .filter(|error| !error.is_empty())
                .map(str::to_string)
        })
        .unwrap_or_else(|| format!("Incus returned HTTP {status}"))
}

// Operation ids and secrets go into a request line, so accept only the
// UUID and hex characters Incus generates.
fn valid_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

/// Reads one remote's settings from the `remotes:` map of an Incus `config.yml`.
pub(crate) fn remote_config(config: &str, remote: &str) -> Option<HashMap<String, String>> {
    let lines = config
        .lines()
        .skip_while(|line| *line != "remotes:")
        .skip(1);
    let mut remote_indent = None;
    let mut field_indent = None;
    let mut fields: Option<HashMap<String, String>> = None;
    for line in lines {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        if indent == 0 {
            break;
        }
        if indent <= *remote_indent.get_or_insert(indent) {
            if fields.is_some() {
                break;
            }
            if trimmed == format!("{remote}:") {
                fields = Some(HashMap::new());
            }
            continue;
        }
        if let Some(fields) = &mut fields
            && indent == *field_indent.get_or_insert(indent)
            && let Some((key, value)) = trimmed.split_once(':')
        {
            let value = value.trim();
            let value = value
                .strip_prefix('"')
                .and_then(|value| value.strip_suffix('"'))
                .or_else(|| {
                    value
                        .strip_prefix('\'')
                        .and_then(|value| value.strip_suffix('\''))
                })
                .unwrap_or_else(|| value.split(" #").next().unwrap_or(value).trim_end());
            fields.insert(key.trim().to_string(), value.to_string());
        }
    }
    fields
}

/// Lists the remote's HTTPS addresses in the order the Incus client tries
/// them: the last working one first, then the configured order.
fn remote_addresses(config: &str, remote: &str) -> Result<Vec<(String, u16)>, String> {
    let fields = remote_config(config, remote)
        .ok_or_else(|| format!("the Incus client configuration has no {remote} remote"))?;
    if fields
        .get("auth_type")
        .is_some_and(|auth| !auth.is_empty() && auth != "tls")
    {
        return Err(format!(
            "the {remote} remote must use TLS client-certificate authentication"
        ));
    }
    let mut addresses: Vec<&str> = fields
        .get("addr")
        .map(|addr| {
            addr.split(',')
                .map(str::trim)
                .filter(|addr| !addr.is_empty())
                .collect()
        })
        .unwrap_or_default();
    if let Some(index) = fields
        .get("last_working_address")
        .and_then(|last| addresses.iter().position(|address| address == last))
    {
        let last = addresses.remove(index);
        addresses.insert(0, last);
    }
    if addresses.is_empty() {
        return Err(format!("the {remote} remote has no address"));
    }
    addresses
        .into_iter()
        .map(|address| parse_https_address(remote, address))
        .collect()
}

fn parse_https_address(remote: &str, address: &str) -> Result<(String, u16), String> {
    let invalid = || {
        format!(
            "the {remote} remote address '{address}' is not an https://HOST[:PORT] URL; the VM display needs the Incus HTTPS listener"
        )
    };
    let authority = address.strip_prefix("https://").ok_or_else(invalid)?;
    let authority = authority.strip_suffix('/').unwrap_or(authority);
    if authority.contains(['/', '?', '#', '@']) {
        return Err(invalid());
    }
    let (host, port) = match authority.strip_prefix('[') {
        Some(bracketed) => {
            let (host, rest) = bracketed.split_once(']').ok_or_else(invalid)?;
            match rest {
                "" => (host, None),
                _ => (host, Some(rest.strip_prefix(':').ok_or_else(invalid)?)),
            }
        }
        None => match authority.split_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        },
    };
    let port = match port {
        Some(port) => port.parse().map_err(|_| invalid())?,
        None => DEFAULT_PORT,
    };
    if host.is_empty() {
        return Err(invalid());
    }
    Ok((host.to_string(), port))
}

#[cfg(test)]
pub(crate) mod test_certs {
    pub(crate) const SERVER_CERT: &str = "-----BEGIN CERTIFICATE-----
MIIBsjCCAVigAwIBAgIUR6I/NN6omJoihHS4vpC/KH5W8KgwCgYIKoZIzj0EAwIw
JTEjMCEGA1UEAwwaY2xhdWRlLXNhbmRib3gtdGVzdC1zZXJ2ZXIwIBcNMjYxMDE3
MDcyNjQyWhgPMjEyNjA5MjMwNzI2NDJaMCUxIzAhBgNVBAMMGmNsYXVkZS1zYW5k
Ym94LXRlc3Qtc2VydmVyMFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAEWbVZZoYC
IJfevQe/hZB/06QiIMImw5J08AJRbr5Mc9H4zKoiOoNo0zL235oGln5MOfwdoAh3
u36nHJJJGCIBCaNkMGIwHQYDVR0OBBYEFP6vD+9cy5EUGwJqUT3j20tQRvd/MB8G
A1UdIwQYMBaAFP6vD+9cy5EUGwJqUT3j20tQRvd/MA8GA1UdEwEB/wQFMAMBAf8w
DwYDVR0RBAgwBocEfwAAATAKBggqhkjOPQQDAgNIADBFAiEAgShpgAhojHuuH3Cd
qtOV1ewK8AcqYg44HdU62OkoFE0CIHNU4zMQrwpj9UE8uJUfkvuZFqMWLTMW6ZBn
2nzteUtq
-----END CERTIFICATE-----
";
    pub(crate) const SERVER_KEY: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgQYzmjz7iuRFhLjC6
ld0Qa/WyvBuZQ1zuAS8R8D4MK32hRANCAARZtVlmhgIgl969B7+FkH/TpCIgwibD
knTwAlFuvkxz0fjMqiI6g2jTMvbfmgaWfkw5/B2gCHe7fqcckkkYIgEJ
-----END PRIVATE KEY-----
";
    pub(crate) const CLIENT_CERT: &str = "-----BEGIN CERTIFICATE-----
MIIBsTCCAVigAwIBAgIUaoRK28QgF5HXJ/kWHe4hW85Eg0swCgYIKoZIzj0EAwIw
JTEjMCEGA1UEAwwaY2xhdWRlLXNhbmRib3gtdGVzdC1jbGllbnQwIBcNMjYxMDE3
MDcyNjQyWhgPMjEyNjA5MjMwNzI2NDJaMCUxIzAhBgNVBAMMGmNsYXVkZS1zYW5k
Ym94LXRlc3QtY2xpZW50MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAE6jbJFJBf
SN62cHsU8igtGVIlx/fjusFORD9+59nIlVBILvTn7HTV0rdnHKoPTT66y9UbagZU
AqWpR86kDCPnX6NkMGIwHQYDVR0OBBYEFJ9ypDurqa7W0ydVCydUlATDnvX5MB8G
A1UdIwQYMBaAFJ9ypDurqa7W0ydVCydUlATDnvX5MA8GA1UdEwEB/wQFMAMBAf8w
DwYDVR0RBAgwBocEfwAAATAKBggqhkjOPQQDAgNHADBEAiB/FEvJLE2CBjmnaEcp
zoHuLjcL3UiLPCK6cRCVil1YEQIgedAJ1RO/ny3mcQooL4CffprvH/uChiAh0/OE
3uF8ll8=
-----END CERTIFICATE-----
";
    pub(crate) const CLIENT_KEY: &str = "-----BEGIN PRIVATE KEY-----
MIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQgzeQddN0uAmoyFHqy
7OOojv4MAbqixYYSFPG/pURXp1OhRANCAATqNskUkF9I3rZwexTyKC0ZUiXH9+O6
wU5EP37n2ciVUEgu9OfsdNXSt2ccqg9NPrrL1RtqBlQCpalHzqQMI+df
-----END PRIVATE KEY-----
";
}

#[cfg(test)]
mod tests {
    use super::test_certs::{CLIENT_CERT, SERVER_CERT};
    use super::*;

    #[test]
    fn pinned_verifier_accepts_only_the_recorded_certificate() {
        let verifier = PinnedServerCert {
            certificate: CertificateDer::from_pem_slice(SERVER_CERT.as_bytes()).unwrap(),
            algorithms: rustls::crypto::aws_lc_rs::default_provider()
                .signature_verification_algorithms,
        };
        let name = ServerName::try_from("127.0.0.1").unwrap();
        let verify = |pem: &str| {
            verifier.verify_server_cert(
                &CertificateDer::from_pem_slice(pem.as_bytes()).unwrap(),
                &[],
                &name,
                &[],
                UnixTime::now(),
            )
        };
        assert!(verify(SERVER_CERT).is_ok());
        assert!(verify(CLIENT_CERT).is_err());
    }

    #[test]
    fn remote_addresses_read_the_named_https_remote() {
        let config = "default-remote: local\nremotes:\n  claude-sandbox:\n    addr: https://[::1] # local listener\n    protocol: incus\n  images:\n    addr: https://images.example:8443\n  local:\n    addr: unix://\naliases: {}\n";
        assert_eq!(
            remote_addresses(config, "claude-sandbox"),
            Ok(vec![("::1".to_string(), DEFAULT_PORT)])
        );
        assert_eq!(
            remote_addresses(
                "remotes:\n  claude-sandbox:\n    addr: https://10.0.0.1:1,https://127.0.0.1:9443,https://10.0.0.2\n    last_working_address: https://127.0.0.1:9443\n",
                "claude-sandbox"
            ),
            Ok(vec![
                ("127.0.0.1".to_string(), 9443),
                ("10.0.0.1".to_string(), 1),
                ("10.0.0.2".to_string(), DEFAULT_PORT),
            ])
        );
        assert!(remote_addresses(config, "local").is_err());
        assert!(remote_addresses(config, "missing").is_err());
    }
}
