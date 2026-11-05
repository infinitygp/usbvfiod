//! Descriptions of the devices that the hotplug commands create, and of the device tree.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::port_path::PortPath;

/// The speed of a device on the bus.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UsbSpeed {
    Low,
    Full,
    High,
    Super,
    SuperPlus,
}

impl fmt::Display for UsbSpeed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Low => "low",
            Self::Full => "full",
            Self::High => "high",
            Self::Super => "super",
            Self::SuperPlus => "super+",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{0:?} is not a speed, use low, full, high, super or super+")]
pub struct ParseSpeedError(String);

impl FromStr for UsbSpeed {
    type Err = ParseSpeedError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "low" => Ok(Self::Low),
            "full" => Ok(Self::Full),
            "high" => Ok(Self::High),
            "super" => Ok(Self::Super),
            "super+" => Ok(Self::SuperPlus),
            other => Err(ParseSpeedError(other.to_string())),
        }
    }
}

/// The configuration of a virtual hub.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HubSpec {
    /// The vendor ID. Required unless `descriptors` are given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vid: Option<u16>,
    /// The product ID. Required unless `descriptors` are given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u16>,
    /// The device release number as BCD, 0x0100 if not given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bcd_device: Option<u16>,
    /// The manufacturer string, no string if not given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub manufacturer: Option<String>,
    /// The product string, no string if not given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub product: Option<String>,
    /// The serial number string, no string if not given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
    /// The number of downstream ports, 1 to 15.
    pub ports: u8,
    /// The speed of the hub: full speed, or high speed with a single transaction translator.
    pub speed: UsbSpeed,
    /// The raw device descriptor followed by the configuration descriptor, as hexadecimal text.
    ///
    /// They are served as they are. The hub class descriptor is generated from `ports` in any
    /// case, and the identity fields above are ignored in favour of the descriptors, while the
    /// strings are still served for the string indexes that the device descriptor names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub descriptors: Option<String>,
}

/// A device of the tree, as listed by the controller.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TreeEntry {
    pub path: PortPath,
    pub speed: UsbSpeed,
    /// The identifier that the device was attached with.
    pub id: String,
    /// The number of downstream ports, for a hub.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hub_ports: Option<u8>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speeds_are_lower_case_words() {
        for (text, speed) in [
            ("low", UsbSpeed::Low),
            ("full", UsbSpeed::Full),
            ("high", UsbSpeed::High),
            ("super", UsbSpeed::Super),
            ("super+", UsbSpeed::SuperPlus),
        ] {
            assert_eq!(text.parse(), Ok(speed));
            assert_eq!(speed.to_string(), text);
        }
        "High".parse::<UsbSpeed>().unwrap_err();
        assert_eq!(serde_json::to_string(&UsbSpeed::High).unwrap(), "\"high\"");
    }

    #[test]
    fn a_hub_spec_without_optional_fields_is_short_json() {
        let spec = HubSpec {
            vid: Some(0x067b),
            pid: Some(0x2586),
            bcd_device: None,
            manufacturer: None,
            product: Some("USB 2.0 Hub".to_string()),
            serial: None,
            ports: 4,
            speed: UsbSpeed::High,
            descriptors: None,
        };

        let json = serde_json::to_string(&spec).unwrap();

        assert_eq!(
            json,
            r#"{"vid":1659,"pid":9606,"product":"USB 2.0 Hub","ports":4,"speed":"high"}"#
        );
        assert_eq!(serde_json::from_str::<HubSpec>(&json).unwrap(), spec);
    }
}
