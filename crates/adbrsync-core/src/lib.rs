//! Scan, plan and transfer engine.
//!
//! The engine is pull-only for now and deliberately whole-file: see the design
//! notes for why rsync's delta algorithm is a poor fit for Android backups.

pub mod checksum;
pub mod entry;
pub mod error;
pub mod filter;
pub mod perf;
pub mod plan;
pub mod scan;
pub mod transfer;

pub use checksum::Digests;
pub use entry::{EntryKind, LocalEntry, RemoteEntry};
pub use error::{Error, Result};
pub use filter::Filter;
pub use perf::PerfReport;
pub use plan::{Plan, PlanOptions, Reason, TransferItem};
pub use scan::{scan_local, scan_remote, RemoteScan};
pub use transfer::{Stats, TransferOptions, TransferReport};
