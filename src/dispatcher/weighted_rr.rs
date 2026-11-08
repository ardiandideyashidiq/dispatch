use std::{
    collections::HashMap,
    fmt::{Display, Formatter},
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    num::NonZeroUsize,
    str::FromStr,
    sync::Arc,
};

use color_eyre::Help;
use eyre::{Context, Result};
use network_interface::NetworkInterfaceConfig;
use tokio::sync::Mutex;
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

                let addresses = get_valid_addresses(&net_interface.addr);

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

            resolved.push(WeightedAddress {
                interface: Interface::Ip(ip),
                weight,
            });
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

            let valid_addrs = get_valid_addresses(&net_interface.addr);
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
                    if can_reach_internet(IpAddr::V4(*v4)).await {
                        online_v4 = Some(*v4);
                        break;
                    }
                }

                let mut online_v6 = None;
                for v6 in &ipv6_addrs {
                    if can_reach_internet(IpAddr::V6(*v6)).await {
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

#[derive(Clone, Debug)]
pub struct WeightedIp {
    ip: IpAddr,
    weight: NonZeroUsize,
}

#[derive(Debug)]
struct WeightedRoundRobinDispatcherInner {
    ipv4: State,
    ipv6: State,
}

#[derive(Debug)]
struct State {
    ips: Vec<WeightedIp>,
    ip_idx: usize,
    count: usize,
}

impl WeightedRoundRobinDispatcherInner {
    fn new(addresses: Vec<WeightedAddress>) -> WeightedRoundRobinDispatcherInner {
        debug_assert!(
            !addresses.is_empty(),
            "dispatcher should have at least one address"
        );

        let mut ipv4s = vec![];
        let mut ipv6s = vec![];

        for address in addresses {
            match address.interface {
                Interface::Named { ipv4, ipv6, .. } => {
                    if let Some(ipv4) = ipv4 {
                        ipv4s.push(WeightedIp {
                            ip: IpAddr::V4(ipv4),
                            weight: address.weight,
                        });
                    }
                    if let Some(ipv6) = ipv6 {
                        ipv6s.push(WeightedIp {
                            ip: IpAddr::V6(ipv6),
                            weight: address.weight,
                        });
                    }
                }
                Interface::Ip(ip) => match ip {
                    IpAddr::V4(v4) => ipv4s.push(WeightedIp {
                        ip: IpAddr::V4(v4),
                        weight: address.weight,
                    }),
                    IpAddr::V6(v6) => ipv6s.push(WeightedIp {
                        ip: IpAddr::V6(v6),
                        weight: address.weight,
                    }),
                },
            }
        }

        WeightedRoundRobinDispatcherInner {
            ipv4: State {
                ips: ipv4s,
                ip_idx: 0,
                count: 0,
            },
            ipv6: State {
                ips: ipv6s,
                ip_idx: 0,
                count: 0,
            },
        }
    }

    fn dispatch(&mut self, remote_addr: &SocketAddr) -> Result<IpAddr> {
        let state = self.select_state(remote_addr)?;

        let ip = &state.ips[state.ip_idx];

        state.count += 1;
        if state.count == usize::from(ip.weight) {
            state.count = 0;
            state.ip_idx = (state.ip_idx + 1) % state.ips.len();
        }

        Ok(ip.ip)
    }

    fn select_state(&mut self, remote_addr: &SocketAddr) -> Result<&mut State> {
        let state = match remote_addr.ip() {
            IpAddr::V4(_) => &mut self.ipv4,
            IpAddr::V6(_) => &mut self.ipv6,
        };

        if state.ips.is_empty() {
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

        Ok(state)
    }
}

#[derive(Debug, Clone)]
pub struct WeightedRoundRobinDispatcher(Arc<Mutex<WeightedRoundRobinDispatcherInner>>);

impl WeightedRoundRobinDispatcher {
    pub fn new(addresses: Vec<WeightedAddress>) -> WeightedRoundRobinDispatcher {
        WeightedRoundRobinDispatcher(Arc::new(Mutex::new(
            WeightedRoundRobinDispatcherInner::new(addresses),
        )))
    }
}

#[async_trait::async_trait]
impl Dispatch for WeightedRoundRobinDispatcher {
    #[instrument]
    async fn dispatch(&self, remote_addr: &SocketAddr) -> Result<IpAddr> {
        let mut dispatcher = self.0.lock().await;
        dispatcher.dispatch(remote_addr)
    }
}

fn addr_type(addr: IpAddr) -> &'static str {
    match addr {
        IpAddr::V4(_) => "IPv4",
        IpAddr::V6(_) => "IPv6",
    }
}
