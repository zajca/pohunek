//! SDK client primitives for pohunek.

#![forbid(unsafe_code)]

mod error;
mod transport;

pub use error::ClientError;
pub use protocol;
pub use transport::{
    connect_raw, connect_raw_local, connect_raw_tcp_addr, Client, ClientOptions, RawStream,
    Subscription,
};
