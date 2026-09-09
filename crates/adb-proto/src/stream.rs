use tokio::io::{AsyncReadExt, AsyncWriteExt, BufStream};
use tokio::net::TcpStream;

use crate::error::{Error, Result};

/// A connection to the adb server.
///
/// The host wire format is a four-hex-digit length prefix followed by an ASCII
/// request; the server answers `OKAY` or `FAIL` plus a length-prefixed reason.
/// After a `host:transport*` request the same socket becomes a raw pipe to the
/// device's adbd, over which device services (`sync:`, `shell,v2,raw:`) are opened.
pub struct AdbStream {
    inner: BufStream<TcpStream>,
}

impl AdbStream {
    pub async fn connect(addr: &str) -> Result<Self> {
        let sock = TcpStream::connect(addr)
            .await
            .map_err(|source| Error::ServerUnreachable {
                addr: addr.to_string(),
                source,
            })?;
        // adb is request/response and latency-sensitive; Nagle would add delay
        // to every small sync request for no benefit.
        sock.set_nodelay(true)?;
        Ok(Self {
            inner: BufStream::new(sock),
        })
    }

    /// Send a request and consume the `OKAY`/`FAIL` status that follows it.
    pub async fn request(&mut self, request: &str) -> Result<()> {
        self.write_request(request).await?;
        self.read_status(request).await
    }

    async fn write_request(&mut self, request: &str) -> Result<()> {
        if request.len() > 0xffff {
            return Err(Error::Protocol(format!(
                "request of {} bytes exceeds the 4-hex-digit length prefix",
                request.len()
            )));
        }
        self.inner
            .write_all(format!("{:04x}{}", request.len(), request).as_bytes())
            .await?;
        self.inner.flush().await?;
        Ok(())
    }

    async fn read_status(&mut self, request: &str) -> Result<()> {
        let mut status = [0u8; 4];
        self.inner.read_exact(&mut status).await?;
        match &status {
            b"OKAY" => Ok(()),
            b"FAIL" => {
                let reason = self.read_length_prefixed().await.unwrap_or_default();
                Err(Error::Rejected {
                    request: request.to_string(),
                    reason,
                })
            }
            other => Err(Error::Protocol(format!(
                "expected OKAY or FAIL, got {:?}",
                String::from_utf8_lossy(other)
            ))),
        }
    }

    /// Read a payload introduced by a four-hex-digit length.
    pub async fn read_length_prefixed(&mut self) -> Result<String> {
        let mut len_buf = [0u8; 4];
        self.inner.read_exact(&mut len_buf).await?;
        let len_str = std::str::from_utf8(&len_buf)
            .map_err(|_| Error::Protocol("length prefix is not ASCII".into()))?;
        let len = usize::from_str_radix(len_str, 16)
            .map_err(|_| Error::Protocol(format!("bad length prefix {len_str:?}")))?;
        let mut body = vec![0u8; len];
        self.inner.read_exact(&mut body).await?;
        Ok(String::from_utf8_lossy(&body).into_owned())
    }

    /// Read everything until the peer closes its side.
    pub async fn read_to_end(&mut self) -> Result<Vec<u8>> {
        let mut buf = Vec::new();
        self.inner.read_to_end(&mut buf).await?;
        Ok(buf)
    }

    pub async fn read_exact(&mut self, buf: &mut [u8]) -> Result<()> {
        self.inner.read_exact(buf).await?;
        Ok(())
    }

    pub async fn write_all(&mut self, buf: &[u8]) -> Result<()> {
        self.inner.write_all(buf).await?;
        Ok(())
    }

    pub async fn flush(&mut self) -> Result<()> {
        self.inner.flush().await?;
        Ok(())
    }
}
