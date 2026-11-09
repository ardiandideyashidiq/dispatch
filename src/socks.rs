use std::{
    collections::HashMap,
    fmt::Debug,
    net::{IpAddr, SocketAddr},
    sync::{LazyLock, RwLock},
    time::{Duration, Instant},
};

use color_eyre::Section;
use eyre::{eyre, Report, Result, WrapErr};
use socksv5::{
    v4::SocksV4Command,
    v5::{SocksV5Command, SocksV5Handshake},
    SocksVersion, SocksVersionError,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite},
    net::{lookup_host, TcpSocket, TcpStream},
};
use tracing::instrument;

use crate::{
    dispatcher::{Dispatch, TargetAddress},
    net::bind_socket,
};

const HTTP_METHODS: [&str; 9] = [
    "GET", "HEAD", "POST", "PUT", "DELETE", "CONNECT", "OPTIONS", "TRACE", "PATCH",
];

#[instrument]
fn assert_supports_noauth(handshake: &SocksV5Handshake) -> Result<()> {
    if !handshake
        .methods
        .contains(&socksv5::v5::SocksV5AuthMethod::Noauth)
    {
        Err(unsupported_auth_error())
    } else {
        Ok(())
    }
}

#[instrument]
fn try_bind_socket(target: &TargetAddress) -> Result<TcpSocket> {
    bind_socket(target.ip, target.interface.as_deref()).map_err(|err| match err.raw_os_error() {
        // Can't assign requested address
        Some(49) => eyre::eyre!(err).wrap_err(unaccessible_local_address_error(&target.ip)),
        _ => eyre::eyre!(err),
    })
}

#[derive(Debug)]
pub struct DnsCache {
    entries: RwLock<HashMap<(String, u16), (SocketAddr, Instant)>>,
    ttl: Duration,
}

impl DnsCache {
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            ttl: Duration::from_secs(60),
        }
    }

    pub fn get(&self, domain: &str, port: u16) -> Option<SocketAddr> {
        let entries = self.entries.read().ok()?;
        let (addr, timestamp) = entries.get(&(domain.to_ascii_lowercase(), port))?;
        if timestamp.elapsed() < self.ttl {
            Some(*addr)
        } else {
            None
        }
    }

    pub fn insert(&self, domain: &str, port: u16, addr: SocketAddr) {
        if let Ok(mut entries) = self.entries.write() {
            entries.insert((domain.to_ascii_lowercase(), port), (addr, Instant::now()));
        }
    }
}

static DNS_CACHE: LazyLock<DnsCache> = LazyLock::new(DnsCache::new);

#[instrument]
async fn lookup(domain: &str, port: u16) -> Result<SocketAddr> {
    if let Some(cached_addr) = DNS_CACHE.get(domain, port) {
        return Ok(cached_addr);
    }

    let addr = lookup_host((domain, port))
        .await
        .map_err(|err| eyre::eyre!(err).wrap_err(resolve_host_error(&domain)))?
        .next()
        .ok_or_else(|| resolve_host_error(&domain))?;

    DNS_CACHE.insert(domain, port, addr);
    Ok(addr)
}

#[derive(Debug)]
pub struct SocksHandshake<R, W, D>
where
    R: AsyncRead + Unpin + Debug,
    W: AsyncWrite + Unpin + Debug,
    D: Dispatch + Debug,
{
    reader: R,
    writer: W,
    dispatcher: D,
}

impl<R, W, D> SocksHandshake<R, W, D>
where
    R: AsyncRead + Unpin + Debug,
    W: AsyncWrite + Unpin + Debug,
    D: Dispatch + Debug,
{
    pub fn new(reader: R, writer: W, dispatcher: D) -> SocksHandshake<R, W, D> {
        SocksHandshake {
            reader,
            writer,
            dispatcher,
        }
    }

    pub async fn handshake(&mut self) -> Result<TcpStream> {
        match socksv5::read_version(&mut self.reader).await {
            Err(err) => Err(self.handle_version_error(err).await),
            Ok(version) => self.handle_handshake_with_version(version).await,
        }
    }

    #[instrument]
    async fn handle_version_error(&mut self, err: SocksVersionError) -> eyre::Report {
        match err {
            SocksVersionError::InvalidVersion(byte) => {
                match byte as char {
                    // HTTP method prefixes
                    'C' | 'G' | 'P' | 'H' | 'D' | 'O' | 'T' => {
                        let mut out = [0u8; 1024];
                        out[0] = byte;
                        match self.reader.read(&mut out[1..]).await {
                            Ok(read) => {
                                let out = String::from_utf8_lossy(&out[..read + 1]);
                                if HTTP_METHODS.iter().any(|method| out.starts_with(method)) {
                                    http_header_error(&out)
                                } else {
                                    err.into()
                                }
                            }
                            Err(read_err) => eyre!(err).wrap_err(read_err),
                        }
                    }
                    _ => err.into(),
                }
            }
            err => err.into(),
        }
    }

    #[instrument]
    async fn handle_handshake_with_version(&mut self, version: SocksVersion) -> Result<TcpStream> {
        match version {
            socksv5::SocksVersion::V5 => {
                let handshake = socksv5::v5::read_handshake_skip_version(&mut self.reader).await?;

                self.handle_auth(&handshake).await?;

                let host = self.handle_request_v5().await?;

                let target = self
                    .dispatcher
                    .dispatch(&host)
                    .wrap_err_with(dispatch_error)?;

                self.handle_connect_v5(host, target).await
            }
            socksv5::SocksVersion::V4 => {
                let host = self.handle_request_v4().await?;

                let target = self
                    .dispatcher
                    .dispatch(&host)
                    .wrap_err_with(dispatch_error)?;

                self.handle_connect_v4(host, target).await
            }
        }
    }

    #[instrument]
    async fn handle_auth(&mut self, handshake: &SocksV5Handshake) -> Result<()> {
        assert_supports_noauth(handshake)?;

        socksv5::v5::write_auth_method(&mut self.writer, socksv5::v5::SocksV5AuthMethod::Noauth)
            .await?;

        Ok(())
    }

    #[instrument]
    async fn handle_request_v5(&mut self) -> Result<SocketAddr> {
        let request = socksv5::v5::read_request(&mut self.reader).await?;

        match request.command {
            socksv5::v5::SocksV5Command::Connect => {
                let host = match request.host {
                    socksv5::v5::SocksV5Host::Ipv4(ip) => {
                        SocketAddr::new(IpAddr::V4(ip.into()), request.port)
                    }
                    socksv5::v5::SocksV5Host::Ipv6(ip) => {
                        SocketAddr::new(IpAddr::V6(ip.into()), request.port)
                    }
                    socksv5::v5::SocksV5Host::Domain(domain) => {
                        let domain = String::from_utf8(domain)?;
                        let mut addr = match lookup(domain.as_str(), request.port).await {
                            Ok(addr) => addr,
                            Err(err) => {
                                socksv5::v5::write_request_status(
                                    &mut self.writer,
                                    socksv5::v5::SocksV5RequestStatus::HostUnreachable,
                                    socksv5::v5::SocksV5Host::Ipv4([0, 0, 0, 0]),
                                    0,
                                )
                                .await?;
                                return Err(err.note(lookup_note()).note(safe_to_ignore_note()));
                            }
                        };
                        addr.set_port(request.port);
                        addr
                    }
                };

                Ok(host)
            }
            cmd => {
                socksv5::v5::write_request_status(
                    &mut self.writer,
                    socksv5::v5::SocksV5RequestStatus::CommandNotSupported,
                    socksv5::v5::SocksV5Host::Ipv4([0, 0, 0, 0]),
                    0,
                )
                .await?;
                Err(unsupported_v5_command_error(&cmd))
            }
        }
    }

    #[instrument]
    async fn handle_connect_v5(
        &mut self,
        address: SocketAddr,
        target: TargetAddress,
    ) -> Result<TcpStream> {
        let server_socket = try_bind_socket(&target)?;

        let server_stream = server_socket.connect(address).await;

        match server_stream {
            Ok(server_stream) => {
                socksv5::v5::write_request_status(
                    &mut self.writer,
                    socksv5::v5::SocksV5RequestStatus::Success,
                    socksv5::v5::SocksV5Host::Ipv4([0, 0, 0, 0]),
                    0,
                )
                .await?;
                Ok(server_stream)
            }
            Err(err) => {
                // Unix error codes.
                // TODO: handle Windows error codes.
                let status = match err.raw_os_error() {
                    // ENETUNREACH
                    Some(101) => socksv5::v5::SocksV5RequestStatus::NetworkUnreachable,
                    // ETIMEDOUT
                    Some(110) => socksv5::v5::SocksV5RequestStatus::TtlExpired,
                    // ECONNREFUSED
                    Some(111) => socksv5::v5::SocksV5RequestStatus::ConnectionRefused,
                    // EHOSTUNREACH
                    Some(113) => socksv5::v5::SocksV5RequestStatus::HostUnreachable,
                    // Unhandled error code
                    _ => socksv5::v5::SocksV5RequestStatus::ServerFailure,
                };
                socksv5::v5::write_request_status(
                    &mut self.writer,
                    status,
                    socksv5::v5::SocksV5Host::Ipv4([0, 0, 0, 0]),
                    0,
                )
                .await?;
                Err(eyre::eyre!(err).wrap_err(connect_error(&address)))
            }
        }
    }

    #[instrument]
    async fn handle_request_v4(&mut self) -> Result<SocketAddr> {
        let request = socksv5::v4::read_request_skip_version(&mut self.reader).await?;

        match request.command {
            socksv5::v4::SocksV4Command::Connect => Ok(match request.host {
                socksv5::v4::SocksV4Host::Ip(ip) => {
                    SocketAddr::new(IpAddr::V4(ip.into()), request.port)
                }
                socksv5::v4::SocksV4Host::Domain(domain) => {
                    let domain = String::from_utf8(domain)?;

                    match lookup(domain.as_str(), request.port).await {
                        Ok(addr) => addr,
                        Err(err) => {
                            socksv5::v4::write_request_status(
                                &mut self.writer,
                                socksv5::v4::SocksV4RequestStatus::Failed,
                                [0, 0, 0, 0],
                                0,
                            )
                            .await?;
                            return Err(err);
                        }
                    }
                }
            }),
            cmd => {
                socksv5::v4::write_request_status(
                    &mut self.writer,
                    socksv5::v4::SocksV4RequestStatus::Failed,
                    [0, 0, 0, 0],
                    0,
                )
                .await?;
                Err(unsupported_v4_command_error(&cmd))
            }
        }
    }

    #[instrument]
    async fn handle_connect_v4(
        &mut self,
        address: SocketAddr,
        target: TargetAddress,
    ) -> Result<TcpStream> {
        let server_socket = try_bind_socket(&target)?;

        let server_stream = server_socket.connect(address).await;

        match server_stream {
            Ok(server_stream) => {
                socksv5::v4::write_request_status(
                    &mut self.writer,
                    socksv5::v4::SocksV4RequestStatus::Granted,
                    [0, 0, 0, 0],
                    0,
                )
                .await?;
                Ok(server_stream)
            }
            Err(err) => {
                socksv5::v4::write_request_status(
                    &mut self.writer,
                    socksv5::v4::SocksV4RequestStatus::Failed,
                    [0, 0, 0, 0],
                    0,
                )
                .await?;
                Err(eyre::eyre!(err).wrap_err(connect_error(&address)))
            }
        }
    }
}

fn connect_error(address: &SocketAddr) -> Report {
    eyre::eyre!(format!("Failed to connect to address `{}`", address))
        .note("This error usually happens when the proxy fails to contact a remote host.")
        .note(safe_to_ignore_note())
}

fn resolve_host_error<T>(host: &T) -> Report
where
    T: Debug,
{
    eyre::eyre!("Failed to resolve the host `{:?}`", *host)
}

fn dispatch_error() -> Report {
    eyre::eyre!("An error occurred during dispatching")
}

fn unsupported_v4_command_error(cmd: &SocksV4Command) -> Report {
    eyre::eyre!("Unsupported SOCKSv4 proxy command `{:?}`", cmd)
}

fn unsupported_v5_command_error(cmd: &SocksV5Command) -> Report {
    eyre::eyre!("Unsupported SOCKSv4 proxy command `{:?}`", cmd)
}

fn unaccessible_local_address_error(addr: &IpAddr) -> Report {
    eyre::eyre!(format!("The local address `{}` is not accessible.", addr)).suggestion(
        "Please ensure that it matches an existing network \
        interface on your computer by inspecting the output of `dispatch list`.",
    )
}

fn http_header_error(out: &str) -> Report {
    let first_http_line = out.split("\r\n").next().unwrap();
    eyre::eyre!(eyre!(
        "The proxy received `{}` ({} additional bytes not shown), which looks like an HTTP \
        request. Please ensure that you have properly configured the proxy as a SOCKS \
        proxy and not an HTTP proxy.",
        first_http_line,
        out.len() - first_http_line.len()
    ))
}

fn unsupported_auth_error() -> Report {
    eyre::eyre!("Only the NOAUTH SOCKS proxy authentication scheme is supported.").suggestion(
        "Please ensure that you haven't provided authentication credentials to your system's \
                proxy configuration.",
    )
}

fn lookup_note() -> &'static str {
    "This error usually happens when an application tries to contact a domain name that does not exist."
}

fn safe_to_ignore_note() -> String {
    use owo_colors::OwoColorize;
    format!("{} {}", "It is safe to ignore in most cases.".bold(), "However, if you notice a degradation in service because of this error, please file an issue.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dns_cache_hit_and_expiration() {
        let mut cache = DnsCache::new();
        cache.ttl = Duration::from_millis(50);

        let addr: SocketAddr = "1.1.1.1:80".parse().unwrap();
        assert_eq!(cache.get("example.com", 80), None);

        cache.insert("EXAMPLE.com", 80, addr);
        // Case-insensitive lookup hit
        assert_eq!(cache.get("example.com", 80), Some(addr));
        assert_eq!(cache.get("different.com", 80), None);

        std::thread::sleep(Duration::from_millis(60));
        // Expired after TTL
        assert_eq!(cache.get("example.com", 80), None);
    }
}
