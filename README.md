# hysteria-rs

这是仓库内的 Rust 复刻工作区。

当前阶段已落地：

- `core`：协议常量、HTTP 认证头、TCP 请求/响应编解码、UDP 消息编解码、UDP 分片/重组
- `extras`：`Salamander` 混淆层
- `app`：CLI 骨架

目标是按 Go 版的 `app / core / extras` 模块边界逐步做行为兼容复刻。

## HTTP/3 / QUIC Access

当前仓库里，访问支持 HTTP/3 的站点时，请按平台使用下面的入口：

- Linux CLI：优先使用 `socks5`。这条入口支持 `SOCKS5 UDP ASSOCIATE`，可以承载原生 QUIC / HTTP/3 流量。
- Linux CLI 内置 `http` proxy：仅适合 TCP 和传统 HTTPS over CONNECT；它不承载原生 HTTP/3 / QUIC。
- Android：使用 managed VPN，也就是 `VpnService -> TUN -> tun2socks -> local SOCKS -> hysteria-core` 这条透明链路。应用无需感知代理，HTTP/3 / QUIC 由系统流量直接进入隧道。

Android managed VPN 的 DNS 路线当前明确是：

- 系统 DNS 指向真实公共 resolver IP，当前使用 `1.1.1.1`
- app 在本地 SOCKS/DNS 路径上拦截这个 DNS 目标，并经隧道访问远端 DoT 上游，返回真实 IP
- 不使用 fake-IP / `mapdns` 技术路线

简化建议：

- 想让 Linux 浏览器稳定访问 H3 网站：把浏览器代理指向本地 `SOCKS5`
- 想让 Android 应用无感使用 H3：开启 app 的 managed VPN

Android 端可复用的 adb 冒烟验证：

```bash
mobile/scripts/android-h3-smoke.sh --device PHP110
```

脚本会自动：

- 重启 app 并拉起 managed VPN
- 等待 `tun0`
- 用 Chrome 打开 Cloudflare trace
- 读取第一次 `http=` 结果
- 先重载，再按需重新打开 trace 页面，直到某次 follow-up 结果为 `http/3`

如果 VPN 已经在运行，只想重新验证协议结果，可以加 `--skip-start`。

## WireGuard Forwarding

Rust CLI 现在支持 `wireguardForwarding` 配置入口。它本质上复用现有 UDP forwarding，但会自动套用更适合 WireGuard 的默认值：

- `mtu: 1280`
- `timeout: 300s`
- `socketReceiveBuffer: 8388608`
- `socketSendBuffer: 8388608`
- `channelDepth: 4096`

示例：

```yaml
server: your-hysteria-server.example.com:443
auth: your-password
tls:
  insecure: false
  ca: /etc/ssl/certs/your-ca.pem

wireguardForwarding:
  - listen: 127.0.0.1:51820
    remote: 203.0.113.10:51820
```

本功能只负责把本地 WireGuard UDP 流量转发到远端 endpoint；WireGuard 接口本身的 `MTU = 1280` 仍需要在你的 `wg-quick` 或等价配置里设置。

完整示例和 `wg0.conf` 参考见：

- `docs/wireguard-forwarding.md`
- `examples/wireguard/client.yaml`
- `examples/wireguard/exported-client.yaml`
- `examples/wireguard/wg0.conf`

导出只包含 YAML 的可运行配置：

```bash
hysteria -c examples/wireguard/client.yaml share --yaml-only
```

导出后的完整形态可参考：

- `examples/wireguard/exported-client.yaml`

## 吞吐记录

已将当前高时延高丢包链路下的吞吐记录整理到：

- `docs/throughput.md`
