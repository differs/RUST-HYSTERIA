# hysteria-rs

这是仓库内的 Rust 复刻工作区。

当前阶段已落地：

- `core`：协议常量、HTTP 认证头、TCP 请求/响应编解码、UDP 消息编解码、UDP 分片/重组
- `extras`：`Salamander` 混淆层
- `app`：CLI 骨架

目标是按 Go 版的 `app / core / extras` 模块边界逐步做行为兼容复刻。

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
