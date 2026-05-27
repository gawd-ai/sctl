//! Shared JSON-line protocol between `sctl` and external comms providers.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Current protocol version. Helpers must return this from `hello`.
pub const PROTOCOL_VERSION: u32 = 1;

/// One request from `sctl` to a provider helper.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommsRequest {
    pub id: String,
    pub method: String,
    #[serde(default)]
    pub params: Value,
}

/// One response from a provider helper.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommsResponse {
    pub id: String,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<CommsError>,
}

/// Provider-side error shape.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommsError {
    pub code: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<Value>,
}

impl CommsResponse {
    #[must_use]
    pub fn ok(id: impl Into<String>, result: Value) -> Self {
        Self {
            id: id.into(),
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    #[must_use]
    pub fn err(id: impl Into<String>, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            ok: false,
            result: None,
            error: Some(CommsError {
                code: code.into(),
                message: message.into(),
                detail: None,
            }),
        }
    }
}

pub mod methods {
    pub const HELLO: &str = "hello";
    pub const DETECT: &str = "detect";
    pub const OPEN: &str = "open";
    pub const STATUS: &str = "status";
    pub const CAPABILITIES: &str = "capabilities";
    pub const LOCATION_POLL: &str = "location.poll";
    pub const LOCATION_DISABLE: &str = "location.disable";
    pub const LINK_POLL: &str = "link.poll";
    pub const LINK_SPEED_TEST: &str = "link.speed_test";
    pub const CELLULAR_SET_BANDS: &str = "cellular.set_bands";
    pub const CELLULAR_SCAN: &str = "cellular.scan";
    pub const RECOVERY_USB_CYCLE: &str = "recovery.usb_cycle";
}

pub mod capabilities {
    pub const LOCATION_GNSS: &str = "location.gnss";
    pub const LINK_CELLULAR: &str = "link.cellular";
    pub const CELLULAR_BAND_CONTROL: &str = "cellular.band_control";
    pub const CELLULAR_SCAN: &str = "cellular.scan";
    pub const RECOVERY_USB_CYCLE: &str = "recovery.usb_cycle";
    pub const RECOVERY_TUNNEL_WATCHDOG: &str = "recovery.tunnel_watchdog";
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_round_trips_as_json_line_payload() {
        let req = CommsRequest {
            id: "42".to_string(),
            method: methods::LINK_POLL.to_string(),
            params: json!({"refresh": true}),
        };
        let raw = serde_json::to_string(&req).unwrap();
        let decoded: CommsRequest = serde_json::from_str(&raw).unwrap();
        assert_eq!(decoded.id, "42");
        assert_eq!(decoded.method, methods::LINK_POLL);
        assert_eq!(decoded.params["refresh"], json!(true));
    }

    #[test]
    fn error_response_uses_stable_shape() {
        let resp = CommsResponse::err("7", "UNSUPPORTED", "missing capability");
        let value = serde_json::to_value(resp).unwrap();
        assert_eq!(value["id"], "7");
        assert_eq!(value["ok"], false);
        assert_eq!(value["error"]["code"], "UNSUPPORTED");
    }
}
