//! Durable alert ingestion and email delivery for the private terminal.

pub mod channel;
pub(crate) mod http;
pub mod model;
pub mod store;
pub mod worker;
