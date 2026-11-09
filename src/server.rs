use std::{
    fmt::Debug,
    net::{IpAddr, SocketAddr},
};

use color_eyre::owo_colors::OwoColorize;
use eyre::Result;
use tokio::net::{TcpListener, TcpStream};
use tracing::instrument;

use crate::{
    dispatcher::{
        Dispatch, RawWeightedAddress, WeightedAddress, WeightedRoundRobinDispatcher,
    },
    socks::SocksHandshake,
};

#[instrument]
async fn handle_socket<D>(mut socket: TcpStream, dispatcher: D) -> Result<()>
where
    D: Dispatch + Debug,
{
    let (mut server_socket, target, dest_display) = {
        let (client_reader, client_writer) = socket.split();

        let mut handshake = SocksHandshake::new(client_reader, client_writer, dispatcher);

        match handshake.handshake().await {
            Err(err) => {
                return Err(err.wrap_err(eyre::eyre!(
                    "An error occurred during the proxy handshake procedure"
                )));
            }
            Ok(result) => result,
        }
    };

    let local_addr = match socket.peer_addr() {
        Ok(local_addr) => local_addr,
        Err(err) => match err.raw_os_error() {
            // InvalidInput: Invalid argument
            // Occurs if the socket was closed in the time it took us to get to this point.
            Some(22) => return Ok(()),
            _ => return Err(err.into()),
        },
    };

    let iface_display = match &target.interface {
        Some(name) => format!("{name} ({})", target.ip),
        None => format!("{}", target.ip),
    };

    println!(
        "{} {} -> {} via {}",
        "[+]".green().bold(),
        local_addr,
        dest_display.bold(),
        iface_display.cyan()
    );

    tracing::info!(
        "connection initiated between {} and {} via {}",
        local_addr,
        dest_display,
        iface_display
    );
    if let Err(err) = tokio::io::copy_bidirectional_with_sizes(
        &mut socket,
        &mut server_socket,
        65536,
        65536,
    )
    .await
    {
        if err.kind() != std::io::ErrorKind::ConnectionReset {
            tracing::warn!(
                "connection error between {} and {}: {}",
                local_addr,
                dest_display,
                err
            );
        }
    }

    tracing::info!(
        "connection terminated between {} and {}",
        local_addr,
        dest_display
    );

    Ok(())
}

#[instrument]
async fn start_server(
    addr: SocketAddr,
    addresses: Vec<WeightedAddress>,
    auto_discovered: bool,
) -> Result<()> {
    let listener = TcpListener::bind(addr).await?;

    if auto_discovered {
        println!(
            "Auto-discovered {} usable {}: {}",
            addresses.len(),
            if addresses.len() > 1 {
                "interfaces"
            } else {
                "interface"
            },
            addresses
                .iter()
                .map(|addr| format!("{}", addr.bold()))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    println!("SOCKS proxy started on {}", addr.bold());
    println!(
        "Dispatching to {} {}",
        if addresses.len() > 1 {
            "addresses"
        } else {
            "address"
        },
        addresses
            .iter()
            .map(|addr| format!("{}", addr.bold()))
            .collect::<Vec<_>>()
            .join(",")
    );

    let dispatcher = WeightedRoundRobinDispatcher::new(addresses);

    loop {
        let (socket, _) = listener.accept().await?;
        let dispatcher = dispatcher.clone();
        tokio::spawn(async move {
            if let Err(err) = handle_socket(socket, dispatcher).await {
                // Errors that happen during the handling of a socket are only reported as warnings, since they're
                // considered to be recoverable. On the other hand, panics are unrecoverable and are reported as errors.
                tracing::warn!("{:?}", err);
            }
        });
    }
}

#[instrument]
pub fn server(ip: IpAddr, port: u16, addresses: Vec<RawWeightedAddress>) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;

    rt.block_on(async {
        let (resolved, auto_discovered) = if addresses.is_empty() {
            (WeightedAddress::auto_discover().await?, true)
        } else {
            (WeightedAddress::resolve(addresses)?, false)
        };

        start_server(SocketAddr::new(ip, port), resolved, auto_discovered).await
    })
}
