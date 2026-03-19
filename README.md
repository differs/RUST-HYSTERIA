# hysteria-rs

这是仓库内的 Rust 复刻工作区。

当前阶段已落地：

- `core`：协议常量、HTTP 认证头、TCP 请求/响应编解码、UDP 消息编解码、UDP 分片/重组
- `extras`：`Salamander` 混淆层
- `app`：CLI 骨架

目标是按 Go 版的 `app / core / extras` 模块边界逐步做行为兼容复刻。
