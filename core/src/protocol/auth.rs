use http::{HeaderMap, HeaderValue};

use crate::{ProtocolError, protocol::padding};

pub const URL_HOST: &str = "hysteria";
pub const URL_PATH: &str = "/auth";

pub const REQUEST_HEADER_AUTH: &str = "hysteria-auth";
pub const RESPONSE_HEADER_UDP_ENABLED: &str = "hysteria-udp";
pub const COMMON_HEADER_CC_RX: &str = "hysteria-cc-rx";
pub const COMMON_HEADER_PADDING: &str = "hysteria-padding";

pub const STATUS_AUTH_OK: u16 = 233;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AuthRequest {
    pub auth: String,
    pub rx: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AuthResponse {
    pub udp_enabled: bool,
    pub rx: u64,
    pub rx_auto: bool,
}

pub fn auth_request_from_headers(headers: &HeaderMap) -> AuthRequest {
    let auth = headers
        .get(REQUEST_HEADER_AUTH)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let rx = headers
        .get(COMMON_HEADER_CC_RX)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);

    AuthRequest { auth, rx }
}

pub fn auth_request_to_headers(
    headers: &mut HeaderMap,
    request: &AuthRequest,
) -> Result<(), ProtocolError> {
    headers.insert(
        REQUEST_HEADER_AUTH,
        HeaderValue::from_str(&request.auth).map_err(|_| ProtocolError::InvalidHeaderValue)?,
    );
    headers.insert(
        COMMON_HEADER_CC_RX,
        HeaderValue::from_str(&request.rx.to_string())
            .map_err(|_| ProtocolError::InvalidHeaderValue)?,
    );
    headers.insert(
        COMMON_HEADER_PADDING,
        HeaderValue::from_str(&padding::auth_request_padding())
            .map_err(|_| ProtocolError::InvalidHeaderValue)?,
    );
    Ok(())
}

pub fn auth_response_from_headers(headers: &HeaderMap) -> AuthResponse {
    let udp_enabled = headers
        .get(RESPONSE_HEADER_UDP_ENABLED)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<bool>().ok())
        .unwrap_or(false);
    let cc_rx = headers
        .get(COMMON_HEADER_CC_RX)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();

    if cc_rx == "auto" {
        AuthResponse {
            udp_enabled,
            rx: 0,
            rx_auto: true,
        }
    } else {
        AuthResponse {
            udp_enabled,
            rx: cc_rx.parse::<u64>().unwrap_or(0),
            rx_auto: false,
        }
    }
}

pub fn auth_response_to_headers(
    headers: &mut HeaderMap,
    response: &AuthResponse,
) -> Result<(), ProtocolError> {
    headers.insert(
        RESPONSE_HEADER_UDP_ENABLED,
        HeaderValue::from_str(if response.udp_enabled {
            "true"
        } else {
            "false"
        })
        .map_err(|_| ProtocolError::InvalidHeaderValue)?,
    );
    let rx_value = if response.rx_auto {
        "auto".to_owned()
    } else {
        response.rx.to_string()
    };
    headers.insert(
        COMMON_HEADER_CC_RX,
        HeaderValue::from_str(&rx_value).map_err(|_| ProtocolError::InvalidHeaderValue)?,
    );
    headers.insert(
        COMMON_HEADER_PADDING,
        HeaderValue::from_str(&padding::auth_response_padding())
            .map_err(|_| ProtocolError::InvalidHeaderValue)?,
    );
    Ok(())
}
