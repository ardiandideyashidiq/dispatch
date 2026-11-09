mod weighted_rr;

use std::net::SocketAddr;

use eyre::Result;

pub use weighted_rr::{
    RawWeightedAddress, TargetAddress, WeightedAddress, WeightedRoundRobinDispatcher,
};
pub trait Dispatch {
    fn dispatch(&self, remote_address: &SocketAddr) -> Result<TargetAddress>;
}
