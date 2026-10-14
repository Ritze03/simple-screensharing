//! Wire protocol types and constants — re-exported from the shared
//! `simple-screensharing-protocol` crate (`protocol/`), which the relay server
//! also uses. Kept as a module so `crate::protocol::X` and
//! `simple_screensharing_client::protocol::X` paths stay valid.

pub use simple_screensharing_protocol::*;
