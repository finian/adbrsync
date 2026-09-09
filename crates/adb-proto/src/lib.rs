//! A direct client for the ADB server protocol.
//!
//! The `adb` executable is never spawned. Beyond avoiding the process cost
//! (measured at ~63 ms per invocation, an order of magnitude more than a
//! protocol round trip), talking to the server directly is what makes several
//! concurrent sync streams possible.

pub mod client;
pub mod error;
pub mod shell;
pub mod stream;
pub mod sync;

pub use client::{AdbClient, DeviceInfo, DeviceSelector, DEFAULT_SERVER_ADDR};
pub use error::{Error, Result};
pub use shell::{shell_quote, ShellOutput};
pub use stream::AdbStream;
pub use sync::{SyncDirEntry, SyncSession, SyncStat};
