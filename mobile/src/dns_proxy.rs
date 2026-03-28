use std::{fmt, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use hysteria_core::{Client, TcpProxyStream};
use rustls::{
    ClientConfig as RustlsClientConfig, RootCertStore,
    pki_types::{CertificateDer, ServerName},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    time::timeout,
};
use tokio_rustls::TlsConnector;

const DNS_HEADER_LEN: usize = 12;
const DNS_TYPE_AAAA: u16 = 28;
const DNS_RCODE_NOERROR: u16 = 0;
const DNS_RCODE_SERVFAIL: u16 = 2;
const DNS_PLAIN_PORT: u16 = 53;
const DNS_DOT_PORT: u16 = 853;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DnsProxyTransport {
    Plain,
    Dot,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum DnsQueryKind {
    Aaaa,
    Other(u16),
}

#[derive(Clone, Debug)]
pub(crate) struct DotUpstream {
    pub address: String,
    pub server_name: String,
}

pub(crate) type DnsFailureNotifier = Arc<dyn Fn(String) + Send + Sync>;

#[derive(Clone)]
pub(crate) struct DnsProxy {
    local_server_ip: String,
    upstreams: Arc<[DotUpstream]>,
    timeout: Duration,
    tls_config: Arc<RustlsClientConfig>,
    client: Client,
    failure_notifier: Option<DnsFailureNotifier>,
}

impl fmt::Debug for DnsProxy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DnsProxy")
            .field("local_server_ip", &self.local_server_ip)
            .field("upstreams", &self.upstreams)
            .field("timeout", &self.timeout)
            .field("client", &self.client)
            .field("failure_notifier", &self.failure_notifier.is_some())
            .finish()
    }
}

impl DnsProxy {
    #[allow(dead_code)]
    pub(crate) fn new(
        client: Client,
        local_server_ip: impl Into<String>,
        root_certificates: Vec<CertificateDer<'static>>,
        upstreams: Vec<DotUpstream>,
        timeout: Duration,
        failure_notifier: Option<DnsFailureNotifier>,
    ) -> Result<Self> {
        if upstreams.is_empty() {
            bail!("at least one DoT upstream must be configured");
        }

        let mut roots = RootCertStore::empty();
        for cert in root_certificates {
            roots
                .add(cert)
                .map_err(|err| anyhow::anyhow!("invalid DoT root certificate: {err}"))?;
        }

        let crypto = RustlsClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_root_certificates(roots)
        .with_no_client_auth();

        Ok(Self {
            local_server_ip: local_server_ip.into(),
            upstreams: Arc::<[DotUpstream]>::from(upstreams),
            timeout,
            tls_config: Arc::new(crypto),
            client,
            failure_notifier,
        })
    }

    pub(crate) fn match_destination(&self, destination: &str) -> Option<DnsProxyTransport> {
        match_dns_proxy_destination(&self.local_server_ip, destination)
    }

    pub(crate) fn connection_close_reason(&self) -> Option<String> {
        self.client.close_reason_text()
    }

    pub(crate) async fn resolve_raw(&self, request: &[u8]) -> Result<Vec<u8>> {
        match classify_dns_query(request)? {
            DnsQueryKind::Aaaa => build_empty_dns_success_response(request),
            DnsQueryKind::Other(_) => {
                let mut last_error = None;
                for upstream in self.upstreams.iter() {
                    match self.query_upstream(upstream, request).await {
                        Ok(response) => return Ok(response),
                        Err(err) => {
                            last_error = Some(format!(
                                "{} via {} failed: {err:#}",
                                upstream.server_name, upstream.address
                            ));
                        }
                    }
                }

                let reason =
                    last_error.unwrap_or_else(|| "no DoT upstreams attempted".to_string());
                eprintln!(
                    "DoT query failed for local DNS proxy target={}: {}",
                    self.local_server_ip, reason
                );
                self.report_failure(reason);
                build_dns_response_with_rcode(request, DNS_RCODE_SERVFAIL)
            }
        }
    }

    pub(crate) async fn open_dot_upstream(&self) -> Result<TcpProxyStream> {
        let mut last_error = None;
        for upstream in self.upstreams.iter() {
            match timeout(self.timeout, self.client.tcp(&upstream.address)).await {
                Ok(Ok(stream)) => return Ok(stream),
                Ok(Err(err)) => {
                    last_error = Some(format!(
                        "{} via {} failed: {err:#}",
                        upstream.server_name, upstream.address
                    ));
                }
                Err(_) => {
                    last_error = Some(format!(
                        "{} via {} timed out opening TCP tunnel",
                        upstream.server_name, upstream.address
                    ));
                }
            }
        }

        let reason = last_error.unwrap_or_else(|| "no DoT upstreams attempted".to_string());
        self.report_failure(reason.clone());
        bail!("failed to open DoT upstream tunnel: {reason}")
    }

    fn report_failure(&self, reason: String) {
        if let Some(notifier) = self.failure_notifier.as_ref() {
            notifier(reason);
        }
    }

    async fn query_upstream(&self, upstream: &DotUpstream, request: &[u8]) -> Result<Vec<u8>> {
        let mut stream = timeout(self.timeout, self.client.tcp(&upstream.address))
            .await
            .context("DoT TCP connect timed out")?
            .with_context(|| format!("failed to open DoT TCP stream to {}", upstream.address))?;

        let server_name = ServerName::try_from(upstream.server_name.clone())
            .map_err(|_| anyhow::anyhow!("invalid DoT server name {}", upstream.server_name))?;
        let connector = TlsConnector::from(self.tls_config.clone());
        let mut tls = timeout(self.timeout, connector.connect(server_name, &mut stream))
            .await
            .context("DoT TLS handshake timed out")?
            .context("DoT TLS handshake failed")?;

        let request_len = u16::try_from(request.len())
            .map_err(|_| anyhow::anyhow!("DNS query exceeds DoT frame size"))?;
        tls.write_all(&request_len.to_be_bytes()).await?;
        tls.write_all(request).await?;
        tls.flush().await?;

        let mut len_buf = [0_u8; 2];
        timeout(self.timeout, tls.read_exact(&mut len_buf))
            .await
            .context("DoT response length timed out")?
            .context("failed to read DoT response length")?;
        let response_len = u16::from_be_bytes(len_buf) as usize;
        let mut response = vec![0_u8; response_len];
        timeout(self.timeout, tls.read_exact(&mut response))
            .await
            .context("DoT response body timed out")?
            .context("failed to read DoT response body")?;
        Ok(response)
    }
}

pub(crate) fn match_dns_proxy_destination(
    local_server_ip: &str,
    destination: &str,
) -> Option<DnsProxyTransport> {
    match split_host_port(destination) {
        Some((host, DNS_PLAIN_PORT)) if host == local_server_ip => Some(DnsProxyTransport::Plain),
        Some((host, DNS_DOT_PORT)) if host == local_server_ip => Some(DnsProxyTransport::Dot),
        _ => None,
    }
}

pub(crate) fn classify_dns_query(packet: &[u8]) -> Result<DnsQueryKind> {
    let question = first_question(packet)?;
    Ok(match question.qtype {
        DNS_TYPE_AAAA => DnsQueryKind::Aaaa,
        other => DnsQueryKind::Other(other),
    })
}

pub(crate) fn build_empty_dns_success_response(query: &[u8]) -> Result<Vec<u8>> {
    build_dns_response_with_rcode(query, DNS_RCODE_NOERROR)
}

fn build_dns_response_with_rcode(query: &[u8], rcode: u16) -> Result<Vec<u8>> {
    let question = first_question(query)?;
    let mut response = query[..question.end_offset].to_vec();
    let request_flags = u16::from_be_bytes([query[2], query[3]]);
    let flags = 0x8000 | 0x0080 | (request_flags & 0x7900) | (request_flags & 0x0130) | rcode;
    response[2..4].copy_from_slice(&flags.to_be_bytes());
    response[6..8].copy_from_slice(&0_u16.to_be_bytes());
    response[8..10].copy_from_slice(&0_u16.to_be_bytes());
    response[10..12].copy_from_slice(&0_u16.to_be_bytes());
    Ok(response)
}

struct DnsQuestion {
    qtype: u16,
    end_offset: usize,
}

fn first_question(packet: &[u8]) -> Result<DnsQuestion> {
    if packet.len() < DNS_HEADER_LEN {
        bail!("DNS packet is truncated");
    }

    let question_count = u16::from_be_bytes([packet[4], packet[5]]);
    if question_count == 0 {
        bail!("DNS packet does not contain a question");
    }

    let mut offset = DNS_HEADER_LEN;
    loop {
        let Some(&label_len) = packet.get(offset) else {
            bail!("DNS question name is truncated");
        };
        if label_len & 0b1100_0000 != 0 {
            bail!("compressed DNS question names are not supported");
        }
        offset += 1;
        if label_len == 0 {
            break;
        }
        offset = offset.saturating_add(label_len as usize);
        if offset > packet.len() {
            bail!("DNS question name exceeds packet length");
        }
    }

    if offset + 4 > packet.len() {
        bail!("DNS question is missing qtype/qclass");
    }

    Ok(DnsQuestion {
        qtype: u16::from_be_bytes([packet[offset], packet[offset + 1]]),
        end_offset: offset + 4,
    })
}

fn split_host_port(destination: &str) -> Option<(&str, u16)> {
    let (host, port) = destination.rsplit_once(':')?;
    let port = port.parse().ok()?;
    Some((host, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_local_dns_proxy_destinations_for_plain_and_dot() {
        assert_eq!(
            match_dns_proxy_destination("127.0.0.1", "127.0.0.1:53"),
            Some(DnsProxyTransport::Plain)
        );
        assert_eq!(
            match_dns_proxy_destination("127.0.0.1", "127.0.0.1:853"),
            Some(DnsProxyTransport::Dot)
        );
        assert_eq!(
            match_dns_proxy_destination("127.0.0.1", "127.0.0.2:53"),
            None
        );
    }

    #[test]
    fn classifies_aaaa_queries_for_empty_local_response() {
        let query = [
            0x12, 0x34, 0x01, 0x20, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x07, b'e',
            b'x', b'a', b'm', b'p', b'l', b'e', 0x03, b'c', b'o', b'm', 0x00, 0x00, 0x1c,
            0x00, 0x01,
        ];

        assert_eq!(
            classify_dns_query(&query).expect("query should parse"),
            DnsQueryKind::Aaaa
        );
    }

    #[test]
    fn synthesizes_empty_success_response_for_aaaa_queries() {
        let query = [
            0xab, 0xcd, 0x01, 0x20, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x07, b'e',
            b'x', b'a', b'm', b'p', b'l', b'e', 0x03, b'c', b'o', b'm', 0x00, 0x00, 0x1c,
            0x00, 0x01,
        ];

        let response = build_empty_dns_success_response(&query)
            .expect("AAAA queries should be answered locally");

        assert_eq!(&response[..2], &[0xab, 0xcd]);
        assert_eq!(u16::from_be_bytes([response[6], response[7]]), 0);
        assert_eq!(u16::from_be_bytes([response[4], response[5]]), 1);
        assert_eq!(u16::from_be_bytes([response[2], response[3]]) & 0x8000, 0x8000);
    }
}
