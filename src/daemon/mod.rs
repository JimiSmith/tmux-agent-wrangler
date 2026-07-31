//! The always-on, multi-tenant daemon. Its socket server and event loop are
//! added in the integration phase; the pure model-building modules live here.

pub mod assoc;
pub mod notify;
pub mod rows;
