use std::fmt;

/// Errors raised while talking to the adb server or a device.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// The adb server answered `FAIL` with a reason.
    #[error("adb server rejected `{request}`: {reason}")]
    Rejected { request: String, reason: String },

    /// The peer sent something the protocol does not allow.
    #[error("protocol violation: {0}")]
    Protocol(String),

    /// The sync service reported a failure for a path.
    #[error("sync failed for {path}: {reason}")]
    Sync { path: String, reason: String },

    #[error("no device connected")]
    NoDevice,

    /// A serial was named explicitly but the server does not know it.
    #[error("no device with serial {serial:?}; connected: {available}")]
    UnknownSerial {
        serial: String,
        available: DeviceList,
    },

    /// The adb server itself could not be reached.
    #[error(
        "cannot reach the adb server at {addr}: {source}\nis it running? try `adb start-server`"
    )]
    ServerUnreachable {
        addr: String,
        #[source]
        source: std::io::Error,
    },

    #[error("more than one device connected; specify one with --serial ({0})")]
    AmbiguousDevice(DeviceList),
}

/// Wrapper so the ambiguous-device error can list the candidates.
#[derive(Debug, Clone)]
pub struct DeviceList(pub Vec<String>);

impl fmt::Display for DeviceList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.join(", "))
    }
}

pub type Result<T> = std::result::Result<T, Error>;
