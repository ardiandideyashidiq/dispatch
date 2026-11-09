# Repository Guidelines

## Project Overview

`dispatch` (`dispatch-proxy` v0.2.4 on Crates.io) is a cross-platform (Linux, macOS, Windows) asynchronous SOCKSv4/SOCKSv5 proxy server written in Rust (2021 edition). It balances outbound client TCP traffic across multiple local network interfaces using Weighted Round Robin scheduling. It was created as a high-performance Rust rewrite of the original Node.js `dispatch-proxy`.

---

## Architecture & Data Flow

```
                     ┌───────────────────────────────────────────────┐
                     │              Client Application               │
                     └──────────────────────┬────────────────────────┘
                                            │ TCP Connect (default 127.0.0.1:1080)
                                            ▼
┌────────────────────────────────────────────────────────────────────────────────────────┐
│ dispatch server (src/server.rs)                                                        │
│                                                                                        │
│  TcpListener::bind() -> accept loop -> tokio::spawn(handle_socket)                     │
│                                                                                        │
│  ┌──────────────────────────────────────────────────────────────────────────────────┐  │
│  │ SocksHandshake (src/socks.rs)                                                    │  │
│  │  1. read_version -> SOCKSv4 or SOCKSv5 (gracefully intercepts raw HTTP requests) │  │
│  │  2. negotiate auth -> NOAUTH supported                                           │  │
│  │  3. parse Connect command & resolve target host (tokio::net::lookup_host)        │  │
│  │  4. query Dispatcher:                                                            │  │
│  │     dispatcher.dispatch(&remote_addr)                                            │  │
│  │       └─ WeightedRoundRobinDispatcher (src/dispatcher/weighted_rr.rs)            │  │
│  │            └─ Selects local IpAddr matching IP family (IPv4/IPv6) by weight      │  │
│  │  5. bind local TcpSocket to selected local interface (src/net.rs)                │  │
│  │  6. connect to target remote address -> TcpStream                                │  │
│  └──────────────────────────────────┬───────────────────────────────────────────────┘  │
│                                     ▼                                                  │
│  tokio::io::copy_bidirectional_with_sizes (src/server.rs)                               │
│   64KB buffer bidirectional piping with TCP half-close coordination                   │
└─────────────────────────────────────┬──────────────────────────────────────────────────┘
                                      │ Bound Outbound Interface (e.g. eth0, wlan0)
                                      ▼
                     ┌───────────────────────────────────────────────┐
                     │                 Remote Target                 │
                     └───────────────────────────────────────────────┘
```

### Key Modules & Flow
1. **Entry & CLI**: `src/main.rs` parses commands (`list` or `start`) via `clap` and initializes diagnostics.
2. **Diagnostics**: `src/debug.rs` configures custom `color-eyre` panic hooks and file-based (`ProjectDirs -> logs.txt`) or stdout non-blocking tracing.
3. **Interface Enumeration**: `src/list.rs` queries OS interfaces via `network-interface` and prints a CLI table.
4. **Address Resolution**: `src/dispatcher/weighted_rr.rs` parses `<interface_or_ip>[/<weight>]` syntax and maps names to bindable non-loopback IPs.
5. **Connection Acceptance**: `src/server.rs` binds `tokio::net::TcpListener`, accepts connections, and spawns isolated tasks.
6. **SOCKS Negotiation & DNS Caching**: `src/socks.rs` decodes SOCKSv4/SOCKSv5 protocols via `socksv5`, queries an in-memory TTL `DnsCache` (falling back to asynchronous DNS lookup), and binds an outbound `tokio::net::TcpSocket` (in `src/net.rs`) to the local IP and device interface chosen by `WeightedRoundRobinDispatcher`.
7. **Bidirectional Tunneling**: `src/server.rs` tunnels client and server data concurrently via `tokio::io::copy_bidirectional_with_sizes` using 64KB buffers.
---

## Key Directories

- `src/`: Main Rust source code.
  - `src/dispatcher/`: Dispatching algorithms and interface definitions (`Dispatch` trait).
- `.github/workflows/`: GitHub Actions CI/CD automation pipelines.

---

## Development Commands

All development and verification tasks are performed using standard Cargo commands:

```bash
# Check compilation and type safety
cargo check

# Run tests
cargo test
cargo test -- --nocapture

# Run clippy (matches CI strict warning policy)
cargo clippy -- -D warnings

# Check code formatting
cargo fmt --all -- --check

# Format code
cargo fmt --all

# Build release binary
cargo build --release

# Inspect local network interfaces
cargo run -- list

# Start proxy server with auto-discovery of usable internet interfaces
cargo run -- start

# Start proxy server with explicit weighted interfaces
cargo run -- start eth0/3 wlan0/1
# Start proxy with explicit bind IP, port, and debug stdout logging
cargo run -- -d start --ip 127.0.0.1 --port 1080 192.168.1.10/2 192.168.1.11/1
```

---

## Code Conventions & Common Patterns

### Error Handling & Reporting
- Use `eyre::Result<T>` and `color_eyre::Help` / `WrapErr` across all modules.
- Provide actionable suggestions on domain-specific errors using `eyre::eyre!(...).suggestion(...)` (e.g., suggesting running `dispatch list` if an interface is unresolvable).
- Log non-fatal connection errors per-socket using `tracing::warn!("{:?}", err)` without aborting the server accept loop.

### Logging & Diagnostics
- Use `tracing::instrument` on functions and handlers that process connections and dispatch routines.
- Panic messages are captured by `DispatchPanicMessage` in `src/debug.rs`, outputting platform metadata and formatted terminal error reports.
- Non-debug runs redirect logs asynchronously to a platform-specific data directory via `tracing_appender::non_blocking`.

### Concurrency & Trait Design
- Dispatch algorithms implement `pub trait Dispatch` returning `Result<TargetAddress>` synchronously without async Mutex locks or `#[async_trait]` boxed future allocations.
- `WeightedRoundRobinDispatcher` uses pre-expanded immutable schedule slices indexed by wait-free `AtomicUsize` cursors (`Relaxed` ordering), keeping IPv4 and IPv6 cursors decoupled.
- Stream forwarding uses `tokio::io::copy_bidirectional_with_sizes(65536, 65536)` for high throughput and proper TCP half-close handling.

### Socket Configuration & Device Binding
- Outbound sockets created in `src/net.rs` enable `set_reuseaddr(true)` and bind to device names (`SO_BINDTODEVICE` on Linux, `IP_BOUND_IF` on macOS) so traffic reliably exits through the designated physical network adapter.
- Filter out loopback (`is_loopback()`) and IPv4/IPv6 link-local addresses when resolving usable network interfaces.
---

## Important Files

| File | Role |
|---|---|
| `Cargo.toml` | Package manifest (`dispatch-proxy` v0.2.4), dependencies, release profiles (`debug = true`, `lto = "thin"`), `cargo-dist` metadata. |
| `src/main.rs` | CLI definition, subcommand routing (`list` vs `start`), and runtime entry point. |
| `src/server.rs` | Tokio runtime management, TCP listener accept loop, and duplex stream forwarding. |
| `src/socks.rs` | SOCKSv4 / SOCKSv5 protocol state machine and connection negotiation. |
| `src/dispatcher/mod.rs` | `Dispatch` async trait abstraction. |
| `src/dispatcher/weighted_rr.rs` | Weighted Round Robin algorithm, address parsing (`RawWeightedAddress`), and state pools. |
| `src/net.rs` | Outbound socket binding and IP filtering utilities. |
| `src/debug.rs` | Panic hooks and `tracing` subscriber initialization. |
| `.github/workflows/test.yml` | CI gate enforcing `cargo check`, `cargo test`, `cargo fmt`, and `cargo clippy -- -D warnings`. |
| `.github/workflows/build.yml` | Cross-platform matrix build across Linux, Windows, macOS on stable, beta, and nightly. |

---

## Runtime/Tooling Preferences

- **Language / Edition**: Rust, Edition 2021.
- **Asynchronous Runtime**: `tokio` v1 (`rt-multi-thread`, `macros`, `net`, `io-util`).
- **Toolchain**: Rust stable (tested on stable, beta, nightly in CI matrix).
- **Package Manager**: Cargo (lockfile format v4 in `Cargo.lock`).
- **Linter & Formatter**: `clippy` and `rustfmt`. CI fails on any warning (`-D warnings`).
- **Release Packaging**: `cargo-dist` v0.0.6.

---

## Testing & QA

### Current Status
- The repository currently contains no unit or integration test files (`tests/` directory does not exist and no `#[cfg(test)]` modules are defined in `src/`).
- CI passes `cargo test` as a sanity gate to ensure harnesses compile.

### Testing Conventions & Strategy
When adding new functionality or modifying existing behavior:
1. **Unit Tests**:
   - Colocate in the relevant module under `src/` using standard conditional compilation:
     ```rust
     #[cfg(test)]
     mod tests {
         use super::*;
         // tests here
     }
     ```
   - Prioritize deterministic state tests:
     - `src/dispatcher/weighted_rr.rs`: Validate weighted selection ratios, IPv4 vs IPv6 routing, and state reset behavior.
     - `src/dispatcher/weighted_rr.rs`: Test string parsing for `RawWeightedAddress` (`<interface>[/<weight>]`).
2. **Protocol & Stream Mocking**:
   - `SocksHandshake<R, W, D>` is generic over `R: AsyncRead`, `W: AsyncWrite`, and `D: Dispatch`.
   - Test SOCKS handshakes using in-memory byte buffers or `tokio::io::duplex` with mock implementations of `Dispatch` without opening real OS sockets.
3. **Integration Tests**:
   - Place end-to-end integration tests in `tests/` (e.g., `tests/socks_proxy.rs`).
4. **Mandatory QA Gate**:
   - Run the full CI validation suite before committing:
     ```bash
     cargo check && cargo fmt --all -- --check && cargo clippy -- -D warnings && cargo test
     ```
