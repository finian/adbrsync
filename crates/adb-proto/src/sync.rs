use tokio::io::AsyncWrite;
use tokio::io::AsyncWriteExt;

use crate::client::{AdbClient, DeviceSelector};
use crate::error::{Error, Result};
use crate::stream::AdbStream;

const ID_STAT_V2: &[u8; 4] = b"STA2";
const ID_LSTAT_V2: &[u8; 4] = b"LST2";
const ID_LIST_V2: &[u8; 4] = b"LIS2";
const ID_DENT_V2: &[u8; 4] = b"DNT2";
const ID_RECV: &[u8; 4] = b"RECV";
const ID_DATA: &[u8; 4] = b"DATA";
const ID_DONE: &[u8; 4] = b"DONE";
const ID_FAIL: &[u8; 4] = b"FAIL";
const ID_QUIT: &[u8; 4] = b"QUIT";

/// Wire size of `sync_stat_v2` minus its leading id.
const STAT_V2_BODY: usize = 68;
/// Wire size of `sync_dent_v2` minus its leading id.
const DENT_V2_BODY: usize = 72;

/// The sync service caps a single `DATA` chunk at 64 KiB.
const SYNC_DATA_MAX: usize = 64 * 1024;

/// Metadata as reported by the sync service's v2 stat.
#[derive(Debug, Clone, Copy, Default)]
pub struct SyncStat {
    pub error: u32,
    pub mode: u32,
    pub size: u64,
    pub mtime: i64,
}

impl SyncStat {
    pub fn exists(&self) -> bool {
        self.error == 0 && self.mode != 0
    }

    pub fn is_dir(&self) -> bool {
        self.mode & 0o170000 == 0o040000
    }

    pub fn is_file(&self) -> bool {
        self.mode & 0o170000 == 0o100000
    }

    pub fn is_symlink(&self) -> bool {
        self.mode & 0o170000 == 0o120000
    }
}

#[derive(Debug, Clone)]
pub struct SyncDirEntry {
    pub name: String,
    pub stat: SyncStat,
}

/// One `sync:` stream. Requests are answered in order and responses carry no
/// request tag, so a single session cannot pipeline; concurrency comes from
/// opening several sessions.
pub struct SyncSession {
    stream: AdbStream,
}

impl SyncSession {
    pub async fn open(client: &AdbClient, selector: &DeviceSelector) -> Result<Self> {
        let stream = client.open_service(selector, "sync:").await?;
        Ok(Self { stream })
    }

    async fn send_request(&mut self, id: &[u8; 4], payload: &[u8]) -> Result<()> {
        let mut header = [0u8; 8];
        header[..4].copy_from_slice(id);
        header[4..].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        self.stream.write_all(&header).await?;
        self.stream.write_all(payload).await?;
        self.stream.flush().await?;
        Ok(())
    }

    /// Stat following symlinks.
    pub async fn stat(&mut self, path: &str) -> Result<SyncStat> {
        self.stat_with(ID_STAT_V2, path).await
    }

    /// Stat *without* following symlinks, so a link is reported as a link.
    pub async fn lstat(&mut self, path: &str) -> Result<SyncStat> {
        self.stat_with(ID_LSTAT_V2, path).await
    }

    async fn stat_with(&mut self, id: &[u8; 4], path: &str) -> Result<SyncStat> {
        self.send_request(id, path.as_bytes()).await?;
        let mut buf = [0u8; 4 + STAT_V2_BODY];
        self.stream.read_exact(&mut buf).await?;
        if &buf[..4] != id {
            return Err(Error::Protocol(format!(
                "expected {}, got {:?}",
                String::from_utf8_lossy(id),
                String::from_utf8_lossy(&buf[..4])
            )));
        }
        Ok(parse_stat_body(&buf[4..]))
    }

    /// List one directory. Cost scales with directory count, not file count,
    /// which is why this is the fallback scanner rather than a per-file stat.
    pub async fn list(&mut self, path: &str) -> Result<Vec<SyncDirEntry>> {
        self.send_request(ID_LIST_V2, path.as_bytes()).await?;
        let mut entries = Vec::new();
        loop {
            let mut buf = [0u8; 4 + DENT_V2_BODY];
            self.stream.read_exact(&mut buf).await?;
            match &buf[..4] {
                id if id == ID_DONE => break,
                id if id == ID_DENT_V2 => {}
                other => {
                    return Err(Error::Protocol(format!(
                        "expected DNT2 or DONE, got {:?}",
                        String::from_utf8_lossy(other)
                    )))
                }
            }
            let stat = parse_stat_body(&buf[4..4 + STAT_V2_BODY]);
            let name_len = u32::from_le_bytes(
                buf[4 + STAT_V2_BODY..4 + DENT_V2_BODY]
                    .try_into()
                    .expect("4 bytes"),
            ) as usize;
            let mut name = vec![0u8; name_len];
            self.stream.read_exact(&mut name).await?;
            let name = String::from_utf8_lossy(&name).into_owned();
            if name == "." || name == ".." {
                continue;
            }
            entries.push(SyncDirEntry { name, stat });
        }
        Ok(entries)
    }

    /// Stream one file from the device into `out`, returning the byte count.
    ///
    /// Note the protocol offers no way to start at an offset, so a partial
    /// transfer cannot be resumed through the sync service.
    pub async fn recv<W>(&mut self, path: &str, out: &mut W) -> Result<u64>
    where
        W: AsyncWrite + Unpin,
    {
        self.send_request(ID_RECV, path.as_bytes()).await?;
        let mut written = 0u64;
        let mut header = [0u8; 8];
        let mut chunk = vec![0u8; SYNC_DATA_MAX];
        loop {
            self.stream.read_exact(&mut header).await?;
            let len = u32::from_le_bytes(header[4..].try_into().expect("4 bytes")) as usize;
            match &header[..4] {
                id if id == ID_DATA => {
                    if len > SYNC_DATA_MAX {
                        return Err(Error::Protocol(format!(
                            "DATA chunk of {len} bytes exceeds the {SYNC_DATA_MAX} byte maximum"
                        )));
                    }
                    self.stream.read_exact(&mut chunk[..len]).await?;
                    out.write_all(&chunk[..len]).await?;
                    written += len as u64;
                }
                id if id == ID_DONE => break,
                id if id == ID_FAIL => {
                    let mut msg = vec![0u8; len];
                    self.stream.read_exact(&mut msg).await?;
                    return Err(Error::Sync {
                        path: path.to_string(),
                        reason: String::from_utf8_lossy(&msg).into_owned(),
                    });
                }
                other => {
                    return Err(Error::Protocol(format!(
                        "expected DATA, DONE or FAIL, got {:?}",
                        String::from_utf8_lossy(other)
                    )))
                }
            }
        }
        Ok(written)
    }

    /// Ask adbd to close the sync session politely.
    pub async fn quit(mut self) -> Result<()> {
        self.send_request(ID_QUIT, &[]).await
    }
}

fn parse_stat_body(body: &[u8]) -> SyncStat {
    debug_assert_eq!(body.len(), STAT_V2_BODY);
    let u32_at = |o: usize| u32::from_le_bytes(body[o..o + 4].try_into().expect("4 bytes"));
    let u64_at = |o: usize| u64::from_le_bytes(body[o..o + 8].try_into().expect("8 bytes"));
    // error, dev, ino, mode, nlink, uid, gid, size, atime, mtime, ctime
    SyncStat {
        error: u32_at(0),
        mode: u32_at(20),
        size: u64_at(36),
        mtime: u64_at(52) as i64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stat_body(mode: u32, size: u64, mtime: i64) -> Vec<u8> {
        let mut b = vec![0u8; STAT_V2_BODY];
        b[20..24].copy_from_slice(&mode.to_le_bytes());
        b[36..44].copy_from_slice(&size.to_le_bytes());
        b[52..60].copy_from_slice(&mtime.to_le_bytes());
        b
    }

    #[test]
    fn parses_stat_fields_at_their_wire_offsets() {
        let s = parse_stat_body(&stat_body(0o100644, 123_456, 1_730_772_582));
        assert_eq!(s.size, 123_456);
        assert_eq!(s.mtime, 1_730_772_582);
        assert!(s.is_file());
        assert!(!s.is_dir());
        assert!(s.exists());
    }

    #[test]
    fn classifies_file_types_from_mode() {
        assert!(parse_stat_body(&stat_body(0o040755, 0, 0)).is_dir());
        assert!(parse_stat_body(&stat_body(0o120777, 0, 0)).is_symlink());
        assert!(!parse_stat_body(&stat_body(0, 0, 0)).exists());
    }
}
