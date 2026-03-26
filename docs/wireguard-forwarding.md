# WireGuard Forwarding

`wireguardForwarding` is the Rust CLI's WireGuard-oriented UDP forwarding entry.
It reuses the existing UDP forwarding runtime, but applies defaults that are
safer for WireGuard traffic out of the box:

- `mtu: 1280`
- `timeout: 300s`
- `socketReceiveBuffer: 8388608`
- `socketSendBuffer: 8388608`
- `channelDepth: 4096`

This feature forwards WireGuard UDP packets to a remote endpoint. It does not
create or manage the WireGuard interface itself.

## Hysteria Client Example

```yaml
server: your-hysteria-server.example.com:443
auth: your-password
tls:
  ca: /etc/ssl/certs/your-ca.pem

wireguardForwarding:
  - listen: 127.0.0.1:51820
    remote: 203.0.113.10:51820
```

Optional overrides:

```yaml
wireguardForwarding:
  - listen: 127.0.0.1:51820
    remote: 203.0.113.10:51820
    mtu: 1280
    timeout: 300s
    socketReceiveBuffer: 8388608
    socketSendBuffer: 8388608
    channelDepth: 4096
```

Run it with:

```bash
hysteria -c client.yaml client
```

Export a reusable YAML-only view of the effective client config with:

```bash
hysteria -c client.yaml share --yaml-only
```

The client will open a local UDP listener on `127.0.0.1:51820` and forward all
WireGuard packets received there to `203.0.113.10:51820` through the Hysteria
connection.

## WireGuard Example

Point your local WireGuard peer at the forwarded local UDP listener:

```ini
[Interface]
PrivateKey = <your-private-key>
Address = 10.0.0.2/32
DNS = 1.1.1.1
MTU = 1280

[Peer]
PublicKey = <remote-public-key>
AllowedIPs = 0.0.0.0/0, ::/0
Endpoint = 127.0.0.1:51820
PersistentKeepalive = 25
```

Bring the interface up as usual:

```bash
wg-quick up wg0
```

## Recommended Setup

1. Start the Hysteria client first.
2. Confirm the UDP forwarding listener is up.
3. Start WireGuard and point `Endpoint` to the local forwarded address.
4. Keep the WireGuard `MTU` aligned with the forwarding config. Default to `1280`.

## Notes

- `mtu` is currently a semantic/defaulting field for this forwarding mode. You
  should still set the same MTU on the real WireGuard interface.
- If you need more throughput and your path is clean, test `1280` and `1360`
  with `hysteria udp-bench`.
- The most relevant validation command is:

```bash
hysteria udp-bench run 127.0.0.1:51820 --packet-size 1280 --target-mbps 100
```
