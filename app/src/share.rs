use anyhow::{Result, bail};
use qrcode::{QrCode, render::unicode};
use serde::Serialize;
use url::form_urlencoded::Serializer;

use crate::config::{
    BandwidthConfig, ClientConfig, HttpConfig, Socks5Config, TcpForwardingEntry,
    UdpForwardingEntry, WireGuardForwardingEntry, normalize_cert_hash, normalize_client_config,
};

pub fn build_share_uri(config: &ClientConfig) -> Result<String> {
    let config = normalize_client_config(config)?;
    let server = config.server.trim();
    if server.is_empty() {
        bail!("server must not be empty");
    }

    let mut pairs = Vec::<(&str, String)>::new();
    match config.obfs.r#type.trim().to_ascii_lowercase().as_str() {
        "" | "plain" => {}
        "salamander" => {
            pairs.push(("obfs", "salamander".to_string()));
            pairs.push(("obfs-password", config.obfs.salamander.password.clone()));
        }
        other => bail!("unsupported obfs.type {other}"),
    }
    if !config.tls.sni.trim().is_empty() {
        pairs.push(("sni", config.tls.sni.clone()));
    }
    if config.tls.insecure {
        pairs.push(("insecure", "1".to_string()));
    }
    if !config.tls.pin_sha256.trim().is_empty() {
        pairs.push(("pinSHA256", normalize_cert_hash(&config.tls.pin_sha256)));
    }

    let mut uri = String::from("hysteria2://");
    if !config.auth.is_empty() {
        uri.push_str(&encode_auth(&config.auth));
        uri.push('@');
    }
    uri.push_str(server);
    uri.push('/');

    pairs.sort_by(|left, right| left.0.cmp(right.0));
    let mut query = Serializer::new(String::new());
    for (key, value) in pairs {
        query.append_pair(key, &value);
    }
    let query = query.finish();
    if !query.is_empty() {
        uri.push('?');
        uri.push_str(&query);
    }
    Ok(uri)
}

pub fn render_qr(data: &str) -> Result<String> {
    let code = QrCode::new(data.as_bytes())?;
    Ok(code.render::<unicode::Dense1x2>().quiet_zone(false).build())
}

pub fn build_share_config_yaml(config: &ClientConfig) -> Result<String> {
    let config = normalize_client_config(config)?;
    let export = ShareConfigExport::from_config(&config);
    Ok(serde_yaml::to_string(&export)?)
}

fn encode_auth(auth: &str) -> String {
    match auth.split_once(':') {
        Some((username, password)) => {
            format!(
                "{}:{}",
                escape_userinfo_component(username),
                escape_userinfo_component(password)
            )
        }
        None => escape_userinfo_component(auth),
    }
}

pub(crate) fn escape_userinfo_component(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    for byte in input.bytes() {
        if should_escape_userinfo(byte) {
            output.push('%');
            output.push(char::from(HEX[(byte >> 4) as usize]));
            output.push(char::from(HEX[(byte & 0x0f) as usize]));
        } else {
            output.push(byte as char);
        }
    }
    output
}

const HEX: &[u8; 16] = b"0123456789ABCDEF";

fn should_escape_userinfo(byte: u8) -> bool {
    !matches!(
        byte,
        b'a'..=b'z'
            | b'A'..=b'Z'
            | b'0'..=b'9'
            | b'-'
            | b'_'
            | b'.'
            | b'~'
            | b'$'
            | b'&'
            | b'+'
            | b','
            | b';'
            | b'='
    )
}

#[derive(Debug, Serialize)]
struct ShareConfigExport {
    server: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    auth: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    tls: Option<ShareTlsExport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    obfs: Option<ShareObfsExport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bandwidth: Option<BandwidthConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    socks5: Option<Socks5Config>,
    #[serde(skip_serializing_if = "Option::is_none")]
    http: Option<HttpConfig>,
    #[serde(skip_serializing_if = "Vec::is_empty", rename = "tcpForwarding")]
    tcp_forwarding: Vec<TcpForwardingEntry>,
    #[serde(skip_serializing_if = "Vec::is_empty", rename = "udpForwarding")]
    udp_forwarding: Vec<ShareUdpForwardingExport>,
    #[serde(skip_serializing_if = "Vec::is_empty", rename = "wireguardForwarding")]
    wireguard_forwarding: Vec<ShareWireGuardForwardingExport>,
}

#[derive(Debug, Serialize)]
struct ShareTlsExport {
    #[serde(skip_serializing_if = "String::is_empty")]
    sni: String,
    #[serde(skip_serializing_if = "is_false")]
    insecure: bool,
    #[serde(skip_serializing_if = "String::is_empty", rename = "pinSHA256")]
    pin_sha256: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    ca: String,
}

#[derive(Debug, Serialize)]
struct ShareObfsExport {
    #[serde(rename = "type")]
    kind: String,
    salamander: ShareSalamanderExport,
}

#[derive(Debug, Serialize)]
struct ShareSalamanderExport {
    password: String,
}

#[derive(Debug, Serialize)]
struct ShareUdpForwardingExport {
    listen: String,
    remote: String,
    #[serde(skip_serializing_if = "is_zero_duration", with = "humantime_serde")]
    timeout: std::time::Duration,
    #[serde(skip_serializing_if = "is_zero_usize", rename = "socketReceiveBuffer")]
    socket_receive_buffer: usize,
    #[serde(skip_serializing_if = "is_zero_usize", rename = "socketSendBuffer")]
    socket_send_buffer: usize,
    #[serde(skip_serializing_if = "is_zero_usize", rename = "channelDepth")]
    channel_depth: usize,
}

#[derive(Debug, Serialize)]
struct ShareWireGuardForwardingExport {
    listen: String,
    remote: String,
    mtu: u32,
    #[serde(with = "humantime_serde")]
    timeout: std::time::Duration,
    #[serde(rename = "socketReceiveBuffer")]
    socket_receive_buffer: usize,
    #[serde(rename = "socketSendBuffer")]
    socket_send_buffer: usize,
    #[serde(rename = "channelDepth")]
    channel_depth: usize,
}

impl ShareConfigExport {
    fn from_config(config: &ClientConfig) -> Self {
        let tls = (!config.tls.sni.is_empty()
            || config.tls.insecure
            || !config.tls.pin_sha256.is_empty()
            || !config.tls.ca.is_empty())
        .then(|| ShareTlsExport {
            sni: config.tls.sni.clone(),
            insecure: config.tls.insecure,
            pin_sha256: config.tls.pin_sha256.clone(),
            ca: config.tls.ca.clone(),
        });

        let obfs = match config.obfs.r#type.trim().to_ascii_lowercase().as_str() {
            "salamander" => Some(ShareObfsExport {
                kind: "salamander".to_string(),
                salamander: ShareSalamanderExport {
                    password: config.obfs.salamander.password.clone(),
                },
            }),
            _ => None,
        };

        let bandwidth = (!config.bandwidth.up.is_empty() || !config.bandwidth.down.is_empty())
            .then(|| config.bandwidth.clone());

        let socks5 = config.socks5.clone();
        let http = config.http.clone();
        let tcp_forwarding = config.tcp_forwarding.clone();
        let udp_forwarding = config
            .udp_forwarding
            .iter()
            .cloned()
            .map(ShareUdpForwardingExport::from)
            .collect();
        let wireguard_forwarding = config
            .wireguard_forwarding
            .iter()
            .map(|entry| ShareWireGuardForwardingExport::from(entry.with_defaults()))
            .collect();

        Self {
            server: config.server.clone(),
            auth: config.auth.clone(),
            tls,
            obfs,
            bandwidth,
            socks5,
            http,
            tcp_forwarding,
            udp_forwarding,
            wireguard_forwarding,
        }
    }
}

impl From<UdpForwardingEntry> for ShareUdpForwardingExport {
    fn from(value: UdpForwardingEntry) -> Self {
        Self {
            listen: value.listen,
            remote: value.remote,
            timeout: value.timeout,
            socket_receive_buffer: value.socket_receive_buffer,
            socket_send_buffer: value.socket_send_buffer,
            channel_depth: value.channel_depth,
        }
    }
}

impl From<WireGuardForwardingEntry> for ShareWireGuardForwardingExport {
    fn from(value: WireGuardForwardingEntry) -> Self {
        Self {
            listen: value.listen,
            remote: value.remote,
            mtu: value.mtu,
            timeout: value.timeout,
            socket_receive_buffer: value.socket_receive_buffer,
            socket_send_buffer: value.socket_send_buffer,
            channel_depth: value.channel_depth,
        }
    }
}

fn is_false(value: &bool) -> bool {
    !*value
}

fn is_zero_duration(value: &std::time::Duration) -> bool {
    value.is_zero()
}

fn is_zero_usize(value: &usize) -> bool {
    *value == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        ClientConfig, ClientObfsConfig, ClientTlsConfig, SalamanderConfig, WireGuardForwardingEntry,
    };

    #[test]
    fn build_share_uri_matches_go_examples() {
        let config = ClientConfig {
            server: "noauth.com".to_string(),
            obfs: ClientObfsConfig {
                r#type: "salamander".to_string(),
                salamander: SalamanderConfig {
                    password: "66ccff".to_string(),
                },
            },
            tls: ClientTlsConfig {
                sni: "crap.cc".to_string(),
                insecure: true,
                pin_sha256: "DEAD:BEEF".to_string(),
                ..Default::default()
            },
            ..Default::default()
        };

        assert_eq!(
            build_share_uri(&config).unwrap(),
            "hysteria2://noauth.com/?insecure=1&obfs=salamander&obfs-password=66ccff&pinSHA256=deadbeef&sni=crap.cc"
        );
    }

    #[test]
    fn build_share_uri_with_auth_keeps_expected_shape() {
        let config = ClientConfig {
            server: "continental.org:4443".to_string(),
            auth: "john:wick".to_string(),
            ..Default::default()
        };

        assert_eq!(
            build_share_uri(&config).unwrap(),
            "hysteria2://john:wick@continental.org:4443/"
        );
    }

    #[test]
    fn build_share_uri_escapes_special_auth_characters_like_go_userinfo() {
        let config = ClientConfig {
            server: "example.com:443".to_string(),
            auth: "john:doe:p@ss/word?".to_string(),
            ..Default::default()
        };

        assert_eq!(
            build_share_uri(&config).unwrap(),
            "hysteria2://john:doe%3Ap%40ss%2Fword%3F@example.com:443/"
        );
    }

    #[test]
    fn escape_userinfo_component_preserves_unreserved_and_allowed_subdelims() {
        assert_eq!(
            escape_userinfo_component("azAZ09-_.~$&+,;="),
            "azAZ09-_.~$&+,;="
        );
    }

    #[test]
    fn share_uri_round_trips_special_auth_characters() {
        let config = ClientConfig {
            server: "example.com:443".to_string(),
            auth: "john:doe:p@ss/wo?rd+ok".to_string(),
            ..Default::default()
        };

        let uri = build_share_uri(&config).unwrap();
        let normalized = normalize_client_config(&ClientConfig {
            server: uri,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(normalized.server, "example.com:443");
        assert_eq!(normalized.auth, "john:doe:p@ss/wo?rd+ok");
    }

    #[test]
    fn share_yaml_exports_wireguard_forwarding_with_defaults() {
        let config = ClientConfig {
            server: "example.com:443".to_string(),
            auth: "hunter2".to_string(),
            tls: ClientTlsConfig {
                insecure: true,
                ..Default::default()
            },
            wireguard_forwarding: vec![WireGuardForwardingEntry {
                listen: "127.0.0.1:51820".to_string(),
                remote: "198.51.100.10:51820".to_string(),
                ..Default::default()
            }],
            ..Default::default()
        };

        let yaml = build_share_config_yaml(&config).unwrap();
        assert!(yaml.contains("wireguardForwarding:"));
        assert!(yaml.contains("listen: 127.0.0.1:51820"));
        assert!(yaml.contains("remote: 198.51.100.10:51820"));
        assert!(yaml.contains("mtu: 1280"));
        assert!(yaml.contains("timeout:"));
        assert!(yaml.contains("socketReceiveBuffer: 8388608"));
        assert!(yaml.contains("socketSendBuffer: 8388608"));
        assert!(yaml.contains("channelDepth: 4096"));
    }
}
