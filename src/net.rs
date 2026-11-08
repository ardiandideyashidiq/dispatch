use std::{
    net::{IpAddr, SocketAddr},
    time::Duration,
};

use network_interface::Addr;
use tokio::net::TcpSocket;
use tracing::instrument;

#[instrument]
pub fn bind_socket(addr: IpAddr) -> std::io::Result<TcpSocket> {
    let socket = match addr {
        IpAddr::V4(_) => TcpSocket::new_v4()?,
        IpAddr::V6(_) => TcpSocket::new_v6()?,
    };

    socket.set_reuseaddr(true)?;
    socket.bind((addr, 0).into())?;

    Ok(socket)
}

pub fn get_valid_addresses(addresses: &[Addr]) -> Vec<IpAddr> {
    addresses
        .iter()
        .map(|addr| addr.ip())
        .filter(|addr| !is_local_address(addr))
        .filter(|addr| bind_socket(*addr).is_ok())
        .collect()
}

fn is_local_address(addr: &IpAddr) -> bool {
    if addr.is_loopback() {
        return true;
    }

    match addr {
        IpAddr::V4(ip) => {
            if ip.is_link_local() {
                return true;
            }
        }
        IpAddr::V6(ip) => {
            // Check for link-local (fe80::/10)
            if ip.segments()[0] & 0xffc0 == 0xfe80 {
                return true;
            }
        }
    }

    false
}

pub fn is_virtual_or_vpn(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("tailscale")
        || lower.starts_with("tun")
        || lower.starts_with("tap")
        || lower.starts_with("utun")
        || lower.starts_with("wg")
        || lower.contains("wireguard")
        || lower.starts_with("docker")
        || lower.starts_with("veth")
        || lower.starts_with("br-")
        || lower.starts_with("bridge")
        || lower.starts_with("virbr")
        || lower.starts_with("dummy")
        || lower.starts_with("lo")
        || lower.starts_with("flannel")
        || lower.starts_with("cni")
        || lower.starts_with("vmnet")
        || lower.starts_with("vboxnet")
        || lower.starts_with("vpn")
        || lower.starts_with("zt")
}

pub async fn can_reach_internet(addr: IpAddr) -> bool {
    let targets: &[(IpAddr, u16)] = match addr {
        IpAddr::V4(_) => &[
            (IpAddr::V4(std::net::Ipv4Addr::new(1, 1, 1, 1)), 80),
            (IpAddr::V4(std::net::Ipv4Addr::new(8, 8, 8, 8)), 53),
        ],
        IpAddr::V6(_) => &[
            (
                IpAddr::V6(std::net::Ipv6Addr::new(
                    0x2606, 0x4700, 0x4700, 0, 0, 0, 0, 0x1111,
                )),
                80,
            ),
            (
                IpAddr::V6(std::net::Ipv6Addr::new(
                    0x2001, 0x4860, 0x4860, 0, 0, 0, 0, 0x8888,
                )),
                53,
            ),
        ],
    };

    for &(target_ip, port) in targets {
        if let Ok(socket) = bind_socket(addr) {
            let connect_fut = socket.connect(SocketAddr::new(target_ip, port));
            if tokio::time::timeout(Duration::from_millis(800), connect_fut)
                .await
                .map(|res| res.is_ok())
                .unwrap_or(false)
            {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_virtual_or_vpn() {
        assert!(is_virtual_or_vpn("tailscale0"));
        assert!(is_virtual_or_vpn("Tailscale"));
        assert!(is_virtual_or_vpn("tun0"));
        assert!(is_virtual_or_vpn("tap1"));
        assert!(is_virtual_or_vpn("utun2"));
        assert!(is_virtual_or_vpn("wg0"));
        assert!(is_virtual_or_vpn("docker0"));
        assert!(is_virtual_or_vpn("veth1234"));
        assert!(is_virtual_or_vpn("br-abc"));
        assert!(is_virtual_or_vpn("virbr0"));

        assert!(!is_virtual_or_vpn("enp7s0f4u1"));
        assert!(!is_virtual_or_vpn("wlp3s0"));
        assert!(!is_virtual_or_vpn("eth0"));
        assert!(!is_virtual_or_vpn("wlan0"));
        assert!(!is_virtual_or_vpn("en0"));
    }
}
