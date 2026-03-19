use anyhow::{Result, bail};
use qrcode::{QrCode, render::unicode};
use url::form_urlencoded::Serializer;

use crate::config::{ClientConfig, normalize_cert_hash, normalize_client_config};

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ClientConfig, ClientObfsConfig, ClientTlsConfig, SalamanderConfig};

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
}
