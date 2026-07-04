//! SDK client primitives for pohunek.

#![forbid(unsafe_code)]

mod error;
mod transport;

pub use error::ClientError;
pub use protocol;
pub use transport::{
    attach_raw, attach_raw_local, attach_raw_local_with_options, attach_raw_tcp_addr,
    attach_raw_tcp_addr_with_options, attach_raw_with_options, connect_raw, connect_raw_local,
    connect_raw_local_with_options, connect_raw_tcp_addr, connect_raw_tcp_addr_with_options,
    connect_raw_with_options, is_local_host, next_request_id, Client, ClientOptions, RawStream,
    Subscription, LOCAL_HOST,
};
