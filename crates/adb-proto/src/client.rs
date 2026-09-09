use std::collections::BTreeSet;

use crate::error::{DeviceList, Error, Result};
use crate::stream::AdbStream;

pub const DEFAULT_SERVER_ADDR: &str = "127.0.0.1:5037";

/// Which device a request is aimed at.
#[derive(Debug, Clone, Default)]
pub enum DeviceSelector {
    /// Whatever single device is connected.
    #[default]
    Any,
    Serial(String),
}

impl DeviceSelector {
    fn transport_request(&self) -> String {
        match self {
            Self::Any => "host:transport-any".to_string(),
            Self::Serial(s) => format!("host:transport:{s}"),
        }
    }

    fn host_request(&self, service: &str) -> String {
        match self {
            Self::Any => format!("host:{service}"),
            Self::Serial(s) => format!("host-serial:{s}:{service}"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub serial: String,
    /// `device`, `unauthorized`, `offline`, ...
    pub state: String,
    pub model: Option<String>,
}

impl DeviceInfo {
    pub fn is_usable(&self) -> bool {
        self.state == "device"
    }
}

/// Handle on the local adb server. Each request opens its own connection,
/// which is also how concurrency is obtained: N connections to the server
/// become N independent streams multiplexed onto the one device transport.
#[derive(Debug, Clone)]
pub struct AdbClient {
    addr: String,
}

impl Default for AdbClient {
    fn default() -> Self {
        Self::new(DEFAULT_SERVER_ADDR)
    }
}

impl AdbClient {
    pub fn new(addr: impl Into<String>) -> Self {
        Self { addr: addr.into() }
    }

    pub fn addr(&self) -> &str {
        &self.addr
    }

    async fn host_query(&self, request: &str) -> Result<String> {
        let mut stream = AdbStream::connect(&self.addr).await?;
        stream.request(request).await?;
        stream.read_length_prefixed().await
    }

    pub async fn server_version(&self) -> Result<u32> {
        let raw = self.host_query("host:version").await?;
        u32::from_str_radix(raw.trim(), 16)
            .map_err(|_| Error::Protocol(format!("bad server version {raw:?}")))
    }

    pub async fn devices(&self) -> Result<Vec<DeviceInfo>> {
        let raw = self.host_query("host:devices-l").await?;
        Ok(raw.lines().filter_map(parse_device_line).collect())
    }

    /// Resolve a selector to a concrete, usable device.
    pub async fn resolve(&self, selector: &DeviceSelector) -> Result<DeviceInfo> {
        let devices = self.devices().await?;
        match selector {
            DeviceSelector::Serial(serial) => {
                let available = DeviceList(devices.iter().map(|d| d.serial.clone()).collect());
                devices
                    .into_iter()
                    .find(|d| &d.serial == serial)
                    .ok_or_else(|| Error::UnknownSerial {
                        serial: serial.clone(),
                        available,
                    })
            }
            DeviceSelector::Any => {
                let mut usable: Vec<_> =
                    devices.into_iter().filter(DeviceInfo::is_usable).collect();
                match usable.len() {
                    0 => Err(Error::NoDevice),
                    1 => Ok(usable.remove(0)),
                    _ => Err(Error::AmbiguousDevice(DeviceList(
                        usable.into_iter().map(|d| d.serial).collect(),
                    ))),
                }
            }
        }
    }

    /// Features common to this host and the device. Note this is an
    /// *intersection*: a feature the host supports may still be absent here.
    pub async fn features(&self, selector: &DeviceSelector) -> Result<BTreeSet<String>> {
        let raw = self.host_query(&selector.host_request("features")).await?;
        Ok(raw
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect())
    }

    /// Bind a fresh connection to the device and open one of its services.
    pub async fn open_service(
        &self,
        selector: &DeviceSelector,
        service: &str,
    ) -> Result<AdbStream> {
        let mut stream = AdbStream::connect(&self.addr).await?;
        stream.request(&selector.transport_request()).await?;
        stream.request(service).await?;
        Ok(stream)
    }
}

fn parse_device_line(line: &str) -> Option<DeviceInfo> {
    let mut parts = line.split_whitespace();
    let serial = parts.next()?.to_string();
    let state = parts.next()?.to_string();
    let model = parts
        .find_map(|p| p.strip_prefix("model:"))
        .map(str::to_string);
    Some(DeviceInfo {
        serial,
        state,
        model,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_devices_l_output() {
        let d = parse_device_line(
            "192.168.0.108:41567    device product:OnePlus6T model:ONEPLUS_A6010 transport_id:1",
        )
        .unwrap();
        assert_eq!(d.serial, "192.168.0.108:41567");
        assert_eq!(d.state, "device");
        assert_eq!(d.model.as_deref(), Some("ONEPLUS_A6010"));
        assert!(d.is_usable());
    }

    #[test]
    fn parses_device_without_model() {
        let d = parse_device_line("emulator-5554\toffline").unwrap();
        assert_eq!(d.serial, "emulator-5554");
        assert!(!d.is_usable());
        assert!(d.model.is_none());
    }

    #[test]
    fn selector_shapes_requests() {
        let any = DeviceSelector::Any;
        assert_eq!(any.transport_request(), "host:transport-any");
        assert_eq!(any.host_request("features"), "host:features");

        let s = DeviceSelector::Serial("abc".into());
        assert_eq!(s.transport_request(), "host:transport:abc");
        assert_eq!(s.host_request("features"), "host-serial:abc:features");
    }
}
