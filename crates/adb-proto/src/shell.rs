use crate::client::{AdbClient, DeviceSelector};
use crate::error::{Error, Result};

const ID_STDOUT: u8 = 1;
const ID_STDERR: u8 = 2;
const ID_EXIT: u8 = 3;

/// Result of a `shell,v2` invocation, with the streams kept apart.
///
/// Separating stderr matters: on some devices toybox fails partway through a
/// tree walk (for example `find -exec ... {} +` hitting ARG_MAX) and still
/// exits successfully, so a truncated result is only detectable on stderr.
#[derive(Debug, Clone)]
pub struct ShellOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: i32,
}

impl ShellOutput {
    pub fn stderr_text(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.stderr)
    }

    pub fn stdout_text(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.stdout)
    }

    /// True when the command exited zero and wrote nothing to stderr.
    pub fn is_clean(&self) -> bool {
        self.exit_code == 0 && self.stderr.is_empty()
    }
}

/// Run a command on the device over the `shell,v2,raw:` service.
///
/// `raw` disables the PTY, so stdout is binary-clean and no CRLF translation
/// happens; `v2` frames stdout, stderr and the exit code separately.
pub async fn run(
    client: &AdbClient,
    selector: &DeviceSelector,
    command: &str,
) -> Result<ShellOutput> {
    let service = format!("shell,v2,raw:{command}");
    let mut stream = client.open_service(selector, &service).await?;

    let mut out = ShellOutput {
        stdout: Vec::new(),
        stderr: Vec::new(),
        exit_code: -1,
    };

    let mut header = [0u8; 5];
    loop {
        match stream.read_exact(&mut header).await {
            Ok(()) => {}
            // adbd closes the socket after the exit packet.
            Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e),
        }
        let id = header[0];
        let len = u32::from_le_bytes([header[1], header[2], header[3], header[4]]) as usize;
        let mut payload = vec![0u8; len];
        stream.read_exact(&mut payload).await?;
        match id {
            ID_STDOUT => out.stdout.extend_from_slice(&payload),
            ID_STDERR => out.stderr.extend_from_slice(&payload),
            ID_EXIT => {
                out.exit_code = payload.first().copied().unwrap_or(255) as i32;
                break;
            }
            // stdin/close-stdin/window-size are host-to-device only; ignore anything else.
            _ => {}
        }
    }
    Ok(out)
}

/// Quote a string for safe interpolation into a device shell command.
///
/// Everything is wrapped in single quotes, with embedded single quotes escaped
/// the classic `'\''` way. Device paths come from user input, so this is the
/// only sanctioned way to build a shell command containing one.
pub fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_plain_paths() {
        assert_eq!(shell_quote("/sdcard/DCIM"), "'/sdcard/DCIM'");
    }

    #[test]
    fn quotes_spaces_and_metacharacters() {
        assert_eq!(
            shell_quote("/sdcard/a b;rm -rf *"),
            "'/sdcard/a b;rm -rf *'"
        );
        assert_eq!(shell_quote("a$(id)`id`"), "'a$(id)`id`'");
    }

    #[test]
    fn escapes_embedded_single_quotes() {
        assert_eq!(shell_quote("it's"), r#"'it'\''s'"#);
    }
}
