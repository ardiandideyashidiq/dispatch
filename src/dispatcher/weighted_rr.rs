use std::{
    collections::HashMap,
    fmt::{Display, Formatter},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    num::NonZeroUsize,
    str::FromStr,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use color_eyre::Help;
use eyre::{Context, Result};
use network_interface::NetworkInterfaceConfig;
use tracing::instrument;

use crate::net::{can_reach_internet, get_valid_addresses, is_virtual_or_vpn};

use super::Dispatch;

#[derive(Clone, Debug)]
pub struct RawWeightedAddress {
    interface: RawInterface,
    weight: NonZeroUsize,
}

impl FromStr for RawWeightedAddress {
    type Err = eyre::Report;

    fn from_str(src: &str) -> Result<Self> {
        let mut items = src.split('/');

        let interface: RawInterface = items.next().unwrap().parse()?;

        let weight = match items.next() {
            Some(priority) => priority.parse()?,
            None => NonZeroUsize::new(1).unwrap(),
        };

        Ok(RawWeightedAddress { interface, weight })
    }
}

#[derive(Clone, Debug)]
pub struct RawInterface(String);

impl RawInterface {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for RawInterface {
    type Err = eyre::Report;

    fn from_str(src: &str) -> Result<Self> {
        Ok(RawInterface(src.to_string()))
    }
}

impl Display for RawInterface {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), std::fmt::Error> {
        self.0.fmt(f)
    }
}

#[derive(Clone, Debug)]
pub enum Interface {
    Named {
        name: String,
        ipv4: Option<Ipv4Addr>,
        ipv6: Option<Ipv6Addr>,
    },
    Ip(IpAddr),
}

#[derive(Clone, Debug)]
pub struct WeightedAddress {
    interface: Interface,
    weight: NonZeroUsize,
}

impl WeightedAddress {
    pub fn resolve(addresses: Vec<RawWeightedAddress>) -> Result<Vec<WeightedAddress>> {
        let interfaces = network_interface::NetworkInterface::show()?;
        let interfaces_by_name = interfaces
            .iter()
            .map(|interface| (interface.name.as_str(), interface))
            .collect::<HashMap<_, _>>();

        let mut resolved = Vec::with_capacity(addresses.len());

        'interfaces: for RawWeightedAddress { interface, weight } in addresses {
            if let Some(net_interface) = interfaces_by_name.get(interface.as_str()) {
                let mut ipv4_addrs = vec![];
                let mut ipv6_addrs = vec![];

                let addresses = get_valid_addresses(&net_interface.addr, Some(&net_interface.name));

                for addr in addresses {
                    match addr {
                        IpAddr::V4(v4) => {
                            ipv4_addrs.push(v4);
                        }
                        IpAddr::V6(v6) => {
                            ipv6_addrs.push(v6);
                        }
                    }
                }

                let ipv4 = if let Some(ipv4) = ipv4_addrs.into_iter().next() {
                    if ipv4.is_loopback() {
                        return Err(eyre::eyre!(
                            "Local address `{}` is a loopback address",
                            ipv4
                        ));
                    }
                    Some(ipv4)
                } else {
                    None
                };

                let ipv6 = if let Some(ipv6) = ipv6_addrs.into_iter().next() {
                    if ipv6.is_loopback() {
                        return Err(eyre::eyre!(
                            "Local address `{}` is a loopback address",
                            ipv6
                        ));
                    }
                    Some(ipv6)
                } else {
                    None
                };

                if ipv4.is_none() && ipv6.is_none() {
                    return Err(eyre::eyre!(
                        "No IP addresses found for network interface `{}`",
                        net_interface.name
                    ));
                }

                resolved.push(WeightedAddress {
                    interface: Interface::Named {
                        name: net_interface.name.clone(),
                        ipv4,
                        ipv6,
                    },
                    weight,
                });
                continue 'interfaces;
            }

            let ip: IpAddr = interface.as_str().parse().with_context(|| {
                format!(
                    "Failed to parse `{}` as an IP address or network interface name",
                    interface.as_str()
                )
            })?;
            let owning_interface = interfaces
                .iter()
                .find(|iface| iface.addr.iter().any(|a| a.ip() == ip));

            if let Some(net_interface) = owning_interface {
                resolved.push(WeightedAddress {
                    interface: Interface::Named {
                        name: net_interface.name.clone(),
                        ipv4: match ip {
                            IpAddr::V4(v4) => Some(v4),
                            _ => None,
                        },
                        ipv6: match ip {
                            IpAddr::V6(v6) => Some(v6),
                            _ => None,
                        },
                    },
                    weight,
                });
            } else {
                resolved.push(WeightedAddress {
                    interface: Interface::Ip(ip),
                    weight,
                });
            }
        }

        Ok(resolved)
    }

    pub async fn auto_discover() -> Result<Vec<WeightedAddress>> {
        let interfaces = network_interface::NetworkInterface::show()?;
        let mut candidates = Vec::new();

        for net_interface in interfaces {
            if is_virtual_or_vpn(&net_interface.name) {
                continue;
            }

            let valid_addrs = get_valid_addresses(&net_interface.addr, Some(&net_interface.name));
            let mut ipv4_addrs = Vec::new();
            let mut ipv6_addrs = Vec::new();

            for addr in valid_addrs {
                match addr {
                    IpAddr::V4(v4) => ipv4_addrs.push(v4),
                    IpAddr::V6(v6) => ipv6_addrs.push(v6),
                }
            }

            if !ipv4_addrs.is_empty() || !ipv6_addrs.is_empty() {
                candidates.push((net_interface.name, ipv4_addrs, ipv6_addrs));
            }
        }

        if candidates.is_empty() {
            return Err(eyre::eyre!(
                "No usable physical network interfaces found. Run `dispatch list` to inspect available interfaces."
            ));
        }

        let mut handles = Vec::new();
        for (name, ipv4_addrs, ipv6_addrs) in candidates {
            handles.push(tokio::spawn(async move {
                let mut online_v4 = None;
                for v4 in &ipv4_addrs {
                    if can_reach_internet(IpAddr::V4(*v4), Some(&name)).await {
                        online_v4 = Some(*v4);
                        break;
                    }
                }

                let mut online_v6 = None;
                for v6 in &ipv6_addrs {
                    if can_reach_internet(IpAddr::V6(*v6), Some(&name)).await {
                        online_v6 = Some(*v6);
                        break;
                    }
                }

                (name, ipv4_addrs, ipv6_addrs, online_v4, online_v6)
            }));
        }

        let mut online = Vec::new();
        let mut fallback = Vec::new();

        for handle in handles {
            if let Ok((name, ipv4_addrs, ipv6_addrs, online_v4, online_v6)) = handle.await {
                if online_v4.is_some() || online_v6.is_some() {
                    online.push(WeightedAddress {
                        interface: Interface::Named {
                            name: name.clone(),
                            ipv4: online_v4,
                            ipv6: online_v6,
                        },
                        weight: NonZeroUsize::new(1).unwrap(),
                    });
                }
                fallback.push(WeightedAddress {
                    interface: Interface::Named {
                        name,
                        ipv4: ipv4_addrs.into_iter().next(),
                        ipv6: ipv6_addrs.into_iter().next(),
                    },
                    weight: NonZeroUsize::new(1).unwrap(),
                });
            }
        }

        if !online.is_empty() {
            Ok(online)
        } else if !fallback.is_empty() {
            println!("Warning: No candidate interfaces reached the internet. Falling back to detected physical interfaces.");
            Ok(fallback)
        } else {
            Err(eyre::eyre!(
                "No usable network interfaces found. Run `dispatch list` to inspect available interfaces."
            ))
        }
    }
}

impl Display for WeightedAddress {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), std::fmt::Error> {
        match &self.interface {
            Interface::Named { name, ipv4, ipv6 } => {
                f.write_fmt(format_args!("{name}/{}", self.weight))?;
                if let Some(ipv4) = ipv4 {
                    f.write_fmt(format_args!(" ({ipv4})"))?;
                }
                if let Some(ipv6) = ipv6 {
                    f.write_fmt(format_args!(" ({ipv6})"))?;
                }
            }
            Interface::Ip(ip) => {
                f.write_fmt(format_args!("{ip}/{}", self.weight))?;
            }
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TargetAddress {
    pub ip: IpAddr,
    pub interface: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WeightedRoundRobinDispatcher {
    ipv4_schedule: Arc<[TargetAddress]>,
    ipv4_cursor: Arc<AtomicUsize>,
    ipv6_schedule: Arc<[TargetAddress]>,
    ipv6_cursor: Arc<AtomicUsize>,
}

impl WeightedRoundRobinDispatcher {
    pub fn new(addresses: Vec<WeightedAddress>) -> WeightedRoundRobinDispatcher {
        debug_assert!(
            !addresses.is_empty(),
            "dispatcher should have at least one address"
        );

        let mut ipv4_schedule = Vec::new();
        let mut ipv6_schedule = Vec::new();

        for address in addresses {
            match address.interface {
                Interface::Named { name, ipv4, ipv6 } => {
                    if let Some(ipv4) = ipv4 {
                        let target = TargetAddress {
                            ip: IpAddr::V4(ipv4),
                            interface: Some(name.clone()),
                        };
                        for _ in 0..usize::from(address.weight) {
                            ipv4_schedule.push(target.clone());
                        }
                    }
                    if let Some(ipv6) = ipv6 {
                        let target = TargetAddress {
                            ip: IpAddr::V6(ipv6),
                            interface: Some(name.clone()),
                        };
                        for _ in 0..usize::from(address.weight) {
                            ipv6_schedule.push(target.clone());
                        }
                    }
                }
                Interface::Ip(ip) => match ip {
                    IpAddr::V4(v4) => {
                        let target = TargetAddress {
                            ip: IpAddr::V4(v4),
                            interface: None,
                        };
                        for _ in 0..usize::from(address.weight) {
                            ipv4_schedule.push(target.clone());
                        }
                    }
                    IpAddr::V6(v6) => {
                        let target = TargetAddress {
                            ip: IpAddr::V6(v6),
                            interface: None,
                        };
                        for _ in 0..usize::from(address.weight) {
                            ipv6_schedule.push(target.clone());
                        }
                    }
                },
            }
        }

        WeightedRoundRobinDispatcher {
            ipv4_schedule: ipv4_schedule.into(),
            ipv4_cursor: Arc::new(AtomicUsize::new(0)),
            ipv6_schedule: ipv6_schedule.into(),
            ipv6_cursor: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl Dispatch for WeightedRoundRobinDispatcher {
    #[instrument]
    fn dispatch(&self, remote_addr: &SocketAddr) -> Result<TargetAddress> {
        match remote_addr.ip() {
            IpAddr::V4(_) => {
                if self.ipv4_schedule.is_empty() {
                    return Err(eyre::eyre!(
                        "Address type mismatch: no configured local address or interface can connect to \
                        remote address `{}` ({}) because the address types are incompatible",
                        remote_addr,
                        addr_type(remote_addr.ip())
                    )
                    .suggestion(format!(
                        "Please ensure that the local addresses or network interfaces you have \
                        configured support {}",
                        addr_type(remote_addr.ip())
                    ))
                    .suggestion(
                        "As a last resort, you can try to disable IPv6 support in the settings of your main \
                        network interface to force your OS to use IPv4 everywhere",
                    ));
                }
                let idx = self.ipv4_cursor.fetch_add(1, Ordering::Relaxed) % self.ipv4_schedule.len();
                Ok(self.ipv4_schedule[idx].clone())
            }
            IpAddr::V6(_) => {
                if self.ipv6_schedule.is_empty() {
                    return Err(eyre::eyre!(
                        "Address type mismatch: no configured local address or interface can connect to \
                        remote address `{}` ({}) because the address types are incompatible",
                        remote_addr,
                        addr_type(remote_addr.ip())
                    )
                    .suggestion(format!(
                        "Please ensure that the local addresses or network interfaces you have \
                        configured support {}",
                        addr_type(remote_addr.ip())
                    ))
                    .suggestion(
                        "As a last resort, you can try to disable IPv6 support in the settings of your main \
                        network interface to force your OS to use IPv4 everywhere",
                    ));
                }
                let idx = self.ipv6_cursor.fetch_add(1, Ordering::Relaxed) % self.ipv6_schedule.len();
                Ok(self.ipv6_schedule[idx].clone())
            }
        }
    }
}

fn addr_type(addr: IpAddr) -> &'static str {
    match addr {
        IpAddr::V4(_) => "IPv4",
        IpAddr::V6(_) => "IPv6",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lock_free_wrr_distribution() {
        let ip1 = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10));
        let ip2 = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 20));
        let addresses = vec![
            WeightedAddress {
                interface: Interface::Named {
                    name: "eth0".into(),
                    ipv4: Some(Ipv4Addr::new(192, 168, 1, 10)),
                    ipv6: None,
                },
                weight: NonZeroUsize::new(3).unwrap(),
            },
            WeightedAddress {
                interface: Interface::Named {
                    name: "wlan0".into(),
                    ipv4: Some(Ipv4Addr::new(192, 168, 1, 20)),
                    ipv6: None,
                },
                weight: NonZeroUsize::new(1).unwrap(),
            },
        ];

        let dispatcher = WeightedRoundRobinDispatcher::new(addresses);
        let remote: SocketAddr = "1.1.1.1:80".parse().unwrap();

        let mut c1 = 0;
        let mut c2 = 0;
        for _ in 0..400 {
            let target = dispatcher.dispatch(&remote).unwrap();
            if target.ip == ip1 {
                assert_eq!(target.interface.as_deref(), Some("eth0"));
                c1 += 1;
            } else if target.ip == ip2 {
                assert_eq!(target.interface.as_deref(), Some("wlan0"));
                c2 += 1;
            }
        }

        assert_eq!(c1, 300);
        assert_eq!(c2, 100);
    }
}
