use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Adb(#[from] adb_proto::Error),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// The device walk could not be trusted to be complete.
    #[error("device scan failed: {0}")]
    ScanFailed(String),

    #[error("cannot read local path {path}: {source}")]
    LocalScan {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error(transparent)]
    Filter(#[from] crate::filter::FilterError),

    /// A device-supplied path that would escape the destination directory.
    #[error("refusing unsafe destination path for {0:?}")]
    UnsafePath(String),
}

pub type Result<T> = std::result::Result<T, Error>;
