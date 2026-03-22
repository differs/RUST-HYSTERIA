# Throughput Notes

## 2026-03-19 - 250 ms RTT + 30% loss

链路模型：

- Linux `netem`
- `250 ms RTT`（单向 `125 ms`）
- 双向随机 `30%` 丢包
- 单条持久 QUIC 连接

当前代码在以下 QUIC 窗口配置下，拿到目前已确认的最好结果：

```yaml
quic:
  initStreamReceiveWindow: 268435456
  maxStreamReceiveWindow: 268435456
  initConnReceiveWindow: 536870912
  maxConnReceiveWindow: 536870912
```

实测最佳值：

- 下载：`267.5 Mbps`
- 上传：`499.9 Mbps`

备注：

- 这组数据说明最大杠杆首先来自更大的 QUIC flow-control window。
- 上传已经超过 `300 Mbps`，下载方向仍未稳定突破 `300 Mbps`。
- 下一阶段工作：fork/vendor `quinn` / `quinn-proto`，针对高 RTT + 高 loss 的下载方向继续调优 BBR / recovery / pacing 行为。

## 2026-03-19 - Quinn 参数化调优

已将 fork 后 Quinn / quinn-proto 的关键调优项参数化，当前支持通过环境变量覆盖：

- `HY_RS_BBR_INITIAL_WINDOW`
- `HY_RS_BBR_INITIAL_WINDOW_PKTS`
- `HY_RS_BBR_STARTUP_GROWTH`
- `HY_RS_BBR_STARTUP_ROUNDS`
- `HY_RS_BBR_STARTUP_TRACE`
- `HY_RS_BBR_STATE_TRACE`
- `HY_RS_BBR_EXIT_ON_RECOVERY`
- `HY_RS_BBR_RECOVER_NON_PERSISTENT`
- `HY_RS_BBR_NON_PERSISTENT_LOSS_FACTOR`
- `HY_RS_BBR_USE_CONN_APP_LIMITED`
- `HY_RS_BBR_APP_LIMITED_SOURCE`
- `HY_RS_BBR_APP_LIMITED_TARGET_PCT`
- `HY_RS_BBR_APP_LIMITED_TRACE`
- `HY_RS_BBR_APP_LIMITED_TRACE_EVERY`
- `HY_RS_BBR_APP_LIMITED_DEBOUNCE`
- `HY_RS_BBR_SAMPLER_APP_LIMITED_EXIT`
- `HY_RS_BBR_SAMPLE_BANDWIDTH`
- `HY_RS_BBR_SEND_RATE_ANCHOR`
- `HY_RS_BBR_ACK_EVENT_BW_FUSION`
- `HY_RS_BBR_MAX_BW_UPDATE`
- `HY_RS_BBR_MAX_BW_APP_LIMITED`
- `HY_RS_BBR_STARTUP_FULL_BW_GATE`
- `HY_RS_BBR_PROBE_RTT_ENTRY`
- `HY_RS_BBR_ROUND_GATING`
- `HY_RS_ACK_ENABLE`
- `HY_RS_ACK_THRESH`
- `HY_RS_ACK_MAX_DELAY_MS`
- `HY_RS_ACK_REORDER_THRESHOLD`
- `HY_RS_ADAPTIVE_GSO_ON_LOSS`
- `HY_RS_ADAPTIVE_GSO_PACKET_THRESHOLD`
- `HY_RS_ADAPTIVE_GSO_BYTES_THRESHOLD`
- `HY_RS_ADAPTIVE_GSO_PERSISTENT_ONLY`
- `HY_RS_PACING_USE_RATE`
- `HY_RS_RATE_PACING_BURST_NS`
- `HY_RS_PACING_MAX_BURST_PKTS`
- `HY_RS_FLOW_CONTROL_DIVISOR`
- `HY_RS_FLOW_CONTROL_MIN_THRESHOLD`
- `HY_RS_FLOW_CONTROL_MAX_THRESHOLD`

### 额外说明

为了复现大窗口吞吐，当前 bench harness 需要同时给 **client 和 server** 配大窗口：

```bash
HY_BENCH_STREAM_RWND=268435456
HY_BENCH_CONN_RWND=536870912
HY_SERVER_STREAM_RWND=268435456
HY_SERVER_CONN_RWND=536870912
```

原因：server 侧 `send_window` 也会从本地 QUIC 窗口配置推导。

### 当前最好观测值

场景仍为：

- `250 ms RTT`（`125 ms` + `125 ms`）
- 双向 `30%` 随机丢包
- 单条持久 QUIC 连接
- `10s x 2` 下载轮次

目前参数化扫出来的**最好下载观测值**仍然是当前默认调优，无额外环境变量覆盖：

- round 1: `222.644428 Mbps`
- round 2: `278.347951 Mbps`

补充：

- `HY_RS_BBR_STARTUP_ROUNDS=8` 的一组复测结果为：
  - round 1: `196.521590 Mbps`
  - round 2: `267.013168 Mbps`
- 目前还没有哪组参数在这个环境下稳定超过默认调优的 `278.347951 Mbps`。

## 2026-03-19 - 深改 `quinn-proto` recovery / BBR 的结果

我继续试了更深层的 `quinn-proto` 修改，主要包括：

- 扩展 congestion callback，把更多 loss 上下文传给 BBR
- 直接改 `BBR` 的 recovery 判定/内部状态结构
- 尝试更 aggressive 的 Data PTO / probe 恢复
- 尝试调整 `ProbeBw` gain cycle

结论：

- 这些“更深”的 recovery / probe 改动 **没有超过** 之前的最好结果
- 其中一部分改动在同样场景下会明显退化下载吞吐，因此已经回撤掉明显有害的版本
- 截至当前，**最好下载观测值仍然是 `278.347951 Mbps`**

当前判断：

- 单靠 `loss threshold / PTO / probe` 这一层微调，已经很难把下载稳定推到 `300+ Mbps`
- 真正下一步更可能需要：
  - 更接近原版/`quic-go` 的 delivery-rate estimator
  - 更贴近 Hysteria 的 Brutal / pacing sender
  - 而不是继续只改 Quinn 现有 recovery 阈值


## 2026-03-19 - `quinn-proto` Brutal sender / pacer 落地

本轮已把更接近原版 Hysteria 的 Brutal 路径直接做到 vendored `quinn-proto`：

- 新增 `congestion::BrutalConfig` / `Brutal`
- 新增 ACK-batch 采样 hook：`Controller::on_ack_event`
- 新增 `PacingBehavior`，把 Brutal 的 pacer 与 Quinn 原有 pacing 路径拆开
- `connection/pacing.rs` 新增 Hysteria 风格的 rate token bucket
- `core/src/quic.rs` 不再自己手搓 Brutal 数学，有限带宽路径改为直接走 vendored Brutal

### 验证结论

这次实现解决的是 **有限带宽协商时的 Brutal sender / pacer 路径**，但它没有把之前的 `278.347951 Mbps` 下载最好值推高。

原因也更清楚了：

- 我之前那组 `250 ms RTT + 30% loss` 的最好下载值，本质上跑的是 **`tx=0` / BBR fallback** 路径
- 原版 Hysteria 在 `tx=0` 时本来也会走 `BBR`，不是 Brutal
- 所以把 Brutal sender/pacer 做得更像原版之后，**并不会直接改变那条“无限带宽下载”热点路径**

### 这轮实测

仍使用：

```bash
HY_BENCH_STREAM_RWND=268435456
HY_BENCH_CONN_RWND=536870912
HY_SERVER_STREAM_RWND=268435456
HY_SERVER_CONN_RWND=536870912
```

#### 1) 无限带宽下载（仍是 BBR 路径）

```text
CONNECTED ... tx=0
SUMMARY round=1 direction=download ... throughput_Mbps=118.826286
SUMMARY round=2 direction=download ... throughput_Mbps=168.447966
```

#### 2) 强制有限带宽，走 Brutal 下载路径

场景：
- server `bandwidth.down: 1000 mbps`
- client `bandwidth_max_rx: 125000000` bytes/s

观测值：

- `245.388946 Mbps`
- `260.068975 Mbps`

另一轮：

- `246.567129 Mbps`
- `230.888364 Mbps`

### 当前判断

这轮已经确认：

1. `quinn-proto` 里的 Brutal sender / pacer 已经真正落地
2. 它不是把 `278 -> 300+` 的主杠杆，因为那条 benchmark 主路径仍是 **BBR fallback**
3. 如果还要继续冲你说的 **300+ Mbps 下载**，下一刀应该重新回到：
   - BBR / delivery-rate estimator
   - ACK / loss sampling
   - recovery / pacing 的 **BBR 路径**


## 2026-03-19 - BBR sampler / estimator 改造（下载主路径）

这轮开始直接改 vendored `quinn-proto` 的 **BBR sampler / estimator**，目标是提升 `tx=0`（BBR fallback）这条真正的下载热点路径。

### 代码改动

- `vendor/quinn-proto/src/congestion/bbr/bw_estimation.rs`
  - 从单纯全局 delta，改成加入 **packet send state** 的采样
  - 同时保留 legacy delta estimator，`get_estimate()` 取两者较高值
- `vendor/quinn-proto/src/congestion.rs`
  - `Controller::on_ack` 增加 `packet_number`
- `vendor/quinn-proto/src/connection/mod.rs`
  - ACK 时把 `packet_number` 传给 controller
- `vendor/quinn-proto/src/congestion/bbr/mod.rs`
  - BBR 使用新的 packet-based sampler
  - 修了一个重要 bug：`on_congestion_event` 里 `lost_packets / lost_bytes` 形参顺序原先是错位的
  - recovery 更接近 Go：在 `!is_at_full_bandwidth` 时不进入 recovery
- `vendor/quinn-proto/src/congestion/bbr/min_max.rs`
  - 新增 `HY_RS_BBR_BW_WINDOW_ROUNDS` 环境变量，便于继续做带宽窗口实验

### 结果

#### 250 ms RTT / 0% loss（下载）

```text
round 1: 251.928192 Mbps
round 2: 450.892725 Mbps
```

这说明新的 BBR sampler / estimator 对 **高 RTT、无丢包** 的下载方向是明显有帮助的。

#### 250 ms RTT / 30% loss（下载）

当前默认窗口（不加额外 BBR env）复测：

```text
round 1: 75.328696 Mbps
round 2: 177.005917 Mbps
```

这说明：

- sampler / estimator 不是完全没效果
- 但 **高丢包场景的主瓶颈已经明显转向 BBR recovery / loss handling**
- 也就是说，继续只改 sampler 的收益已经有限，下一刀应该更直接地改 recovery / loss path

### 额外实验

- `HY_RS_BBR_RECOVER_NON_PERSISTENT=false`：没有带来更好结果
- `HY_RS_BBR_BW_WINDOW_ROUNDS=30`：也没有优于默认值

### 当前结论

- 这轮已经把 BBR 的 sampler / estimator 真正往前推了一步
- 但在 **250 ms RTT + 30% loss** 下，它还没有重新回到之前历史最好 `278.347951 Mbps`
- 下一步更应该直接攻：
  - `BBR recovery`
  - `loss response`
  - `ProbeBw / pacing` 在高 loss 下的行为


## 2026-03-20 - BBR recovery / ProbeBw / loss response

继续直接改了 vendored `quinn-proto` 的 BBR 高 loss 路径，主要包括：

- `vendor/quinn-proto/src/congestion/bbr/mod.rs`
  - STARTUP 遇到足够明显的 loss 提前退出
  - recovery 初始化种子改为考虑 `bytes_in_flight_before_loss`
  - `ProbeBw` 在高增益探测阶段一旦看到 loss，立即推进 cycle
  - 新增 lossy ProbeBw gain cycle，临时从 `1.25/0.75` 改为更平缓的 `1.05/0.95`
- `vendor/quinn-proto/src/congestion/bbr/bw_estimation.rs`
  - 当前保留 packet-based + legacy 双 estimator 路径

### 复测结果（250 ms RTT / 30% loss / 下载）

同样的大窗口条件下，这轮多次复测没有稳定超过历史最好值 `278.347951 Mbps`。

本轮里我观察到的最好一组是：

```text
round 1: 123.641680 Mbps
round 2: 211.515280 Mbps
```

但重复复测波动很大，也出现了：

```text
round 1: 91.668285 Mbps
round 2: 170.482609 Mbps
```

以及更差的样本（例如 `127.874256 Mbps` / `145.058776 Mbps`）。

### 当前判断

- 这轮修改 **改善了 0% loss 的高 RTT 下载路径**
- 但在 **30% loss** 下仍然没有把下载重新拉回 `278+ Mbps`
- 当前瓶颈仍然集中在：
  - BBR 的 loss recovery 行为
  - estimator 与 recovery 的耦合
  - ProbeBw 在高随机丢包下的稳定性

也就是说，继续只做局部调参/局部条件分支，收益已经越来越有限；
如果还要继续冲 `300+ Mbps`，更像是要往 **更完整的 quic-go / Hysteria BBR 语义迁移** 走。 

## 2026-03-20 - 继续搬 Go `bbr_sender.go` / `bandwidth_sampler.go` 语义

这轮继续往 vendored `quinn-proto` 里搬了更多 Go 版 BBR / sampler 语义，重点包括：

- ACK-event 粒度的 `AckedPacketInfo` / `LostPacketInfo` 批处理
- 更接近 Go `bandwidth_sampler.go` 的 packet send-state 采样器
- `maxAckHeightTracker` / `extraAcked` / ack aggregation epoch 基础实现
- `recentAckPoints` / `A0 candidates` 基础实现
- `connection/mod.rs` 的 `largest_packet_num_acked` 改成当前 ack event 的最大 newly-acked packet
- 把更多 `OnCongestionEventEx` 逻辑直接搬到 Rust `Bbr::on_ack_event`
- BBR metrics 新增更接近 Go 的 rate-based pacer 输出路径

### 验证

```bash
cd /root/workspaces/hysteria/rust
cargo test
```

结果：**通过**。

### 当前压测结论

同样是：

- `250 ms RTT`（`125 ms + 125 ms`）
- 双向 `30%` 随机丢包
- 大窗口：

```bash
HY_BENCH_STREAM_RWND=268435456
HY_BENCH_CONN_RWND=536870912
HY_SERVER_STREAM_RWND=268435456
HY_SERVER_CONN_RWND=536870912
```

当前这轮“更接近 Go 语义”的直接迁移，**还没有把下载拉回历史最好值**，而且目前仍然显著低于此前的 `278.347951 Mbps` 峰值。

一组代表性结果：

```text
HY_RS_BBR_INITIAL_WINDOW_PKTS=512 HY_RS_BBR_STARTUP_ROUNDS=6
round 1: 0.055257 Mbps
round 2: 109.901373 Mbps
```

另一个无额外 BBR env 的复测结果：

```text
round 1: 5.976649 Mbps
round 2: 146.326791 Mbps
```

### 当前判断

这说明：

- 仅把 Go 的 `bbr_sender.go` / `bandwidth_sampler.go` 逻辑逐段搬到 Quinn 上，**还不足以直接复现**原版 Hysteria / quic-go 在这条链路上的表现。
- 当前剩下的关键差距，已经不只是 BBR sender 本身，还包括：
  - Quinn / quic-go 在丢包恢复、ACK 驱动和 pacing 交互上的实现差异
  - Quinn 连接栈把 congestion callback 分拆成多个阶段后的语义差异
  - Rust 这边的 pacer / recovery 与 quic-go send loop 的整体耦合差异

截至当前，历史最好下载观测值仍然是：

- **`278.347951 Mbps`**


## 2026-03-20 - 继续攻 Quinn sent-packet / pacing coupling

这轮继续往 **Quinn 发送循环 / pacer 耦合层** 下手，重点不是再拧 BBR 参数，而是修 Quinn 侧 pacing 与当前发送批次大小之间的错配。

### 代码侧改动

- `vendor/quinn-proto/src/connection/pacing.rs`
  - 修了 **rate token bucket 在 pacing rate 变化时反复 reset budget** 的问题
  - 对 Quinn `Window` pacing + `pacing_rate` 路径做了更接近 datagram 级 pacing 的处理，避免被当前 `bytes_to_send` / builder 状态过度放大
- `vendor/quinn-proto/src/congestion/bbr/mod.rs`
  - `on_sent` 的 `bytes_in_flight` 语义改得更接近 Go（按发送前 inflight 记录）
  - BBR metrics 改回更适合 Quinn 自身 send loop 的 `Window` pacing 行为，而不是强制单独的 rate-token-bucket 路径
- `vendor/quinn-proto/src/congestion/bbr/bw_estimation.rs`
  - 在当前 Go 风格 sampler 上，重新加回了一个 **legacy ack-delta estimator** 作为补充，以减少纯 packet-sampler 在 Quinn 上的低估

### 验证

```bash
cd /root/workspaces/hysteria/rust
cargo test
```

结果：**通过**。

### 本轮实测

同样使用：

```bash
HY_BENCH_STREAM_RWND=268435456
HY_BENCH_CONN_RWND=536870912
HY_SERVER_STREAM_RWND=268435456
HY_SERVER_CONN_RWND=536870912
```

#### 250 ms RTT / 0% loss / 下载 / 5s x 1

```text
83.648254 Mbps
```

#### 250 ms RTT / 30% loss / 下载 / 10s x 2

```text
round 1: 10.298441 Mbps
round 2: 137.407915 Mbps
```

### 当前判断

这轮可以确认一件事：

- **Quinn 的 pacing 与 send loop 耦合确实是问题的一部分**，而且单独的 Go 风格 token-bucket pacing 直接套在 Quinn 上并不好使。

但同时也确认：

- **真正限制 250ms + 30% loss 下载的，不只是 pacer**。
- 现在即便修了这一层，结果仍然没有回到此前的历史最好值 `278.347951 Mbps`。

也就是说，后续如果还要继续往 `278 -> 300+ Mbps` 冲，下一步更应该继续看：

- Quinn 的 loss-detection / sent-packet bookkeeping
- ACK/loss batch 与 inflight 更新时机
- recovery 与 pacing 的联动


## 2026-03-20 - 继续攻 Quinn loss / recovery 主链路

这轮没有再只盯 pacer，而是直接下到 **loss/recovery** 路径：

- `core/src/quic.rs`
  - 新增环境变量覆盖：
    - `HY_RS_PACKET_THRESHOLD`
    - `HY_RS_TIME_THRESHOLD`
    - `HY_RS_PERSISTENT_CONGESTION_THRESHOLD`
    - `HY_RS_DISABLE_GSO`
- `vendor/quinn-proto/src/congestion/bbr/mod.rs`
  - 将 `recover_on_non_persistent_loss` 的默认值改为 **false**
- `core/src/quic.rs`
  - Rust 侧默认 BBR fallback 也改成 **非持久性 loss 不进入 recovery**

### 关键发现

真正最有用的不是继续调 `packet_threshold / time_threshold`，而是：

- **高随机丢包 (`30%`) 下，不要因为每次 non-persistent loss 都进入 BBR recovery**

也就是说，当前 Quinn + Rust 版里，**non-persistent loss recovery 太激进**，是下载方向的重要瓶颈之一。

### 实测

场景：

- `250 ms RTT`（`125 + 125 ms`）
- 双向 `30%` 随机丢包
- 大窗口：

```bash
HY_BENCH_STREAM_RWND=268435456
HY_BENCH_CONN_RWND=536870912
HY_SERVER_STREAM_RWND=268435456
HY_SERVER_CONN_RWND=536870912
```

#### 仅禁用 non-persistent loss recovery

```bash
HY_RS_BBR_RECOVER_NON_PERSISTENT=0
```

观测值：

```text
round 1: 70.971530 Mbps
round 2: 222.331293 Mbps
```

这比这轮前面的默认路径明显更高，说明主问题确实在 **loss/recovery 响应过重**。

#### 仅把 non-persistent loss factor 设为 0

```bash
HY_RS_BBR_NON_PERSISTENT_LOSS_FACTOR=0.0
```

观测值：

```text
round 1: 42.547784 Mbps
round 2: 179.341391 Mbps
```

说明：

- 单纯把 recovery window 的损失扣减因子降到 0，
- **不如直接禁止 non-persistent loss 进入 recovery**。

#### 改 `packet_threshold / time_threshold`

尝试过：

- `HY_RS_PACKET_THRESHOLD=8 HY_RS_TIME_THRESHOLD=2.0`
- `HY_RS_PACKET_THRESHOLD=16 HY_RS_TIME_THRESHOLD=2.0`
- `HY_RS_TIME_THRESHOLD=4.0`
- 以及与 `HY_RS_DISABLE_GSO=1` 组合

结果都没有超过上面的：

```text
HY_RS_BBR_RECOVER_NON_PERSISTENT=0  -> round 2: 222.331293 Mbps
```

### 当前结论

这轮已经明确：

1. **Quinn loss/recovery 主链路确实是瓶颈之一**
2. 其中最显著的点是：
   - **non-persistent loss recovery 过于激进**
3. 仅靠 `packet_threshold / time_threshold` 调整，收益有限
4. 当前这轮最好新结果是：
   - **`222.331293 Mbps`**（`HY_RS_BBR_RECOVER_NON_PERSISTENT=0`）

虽然还没重新超过历史最好 `278.347951 Mbps`，但这轮已经把“问题在哪”进一步坐实到了 recovery 策略本身。


## 2026-03-20 - recovery gating 继续修正

继续在 BBR recovery 侧做了一轮更直接的修正：

- `vendor/quinn-proto/src/congestion/bbr/mod.rs`
  - `update_recovery_state()` 现在真正根据
    - `persistent_congestion`
    - `recover_on_non_persistent_loss`
    决定是否进入 recovery
  - `calculate_recovery_window()` 重新按配置决定 `effective_recovery_loss`
  - `on_congestion_event()` 只有在 **真的会进入 recovery** 时，才累计用于 startup-loss 退出判定的 loss event / lost bytes

### 结果

#### 默认当前分支（GSO 开）

`250 ms RTT + 30% loss` 下载：

```text
round 1: 42.323817 Mbps
round 2: 176.324705 Mbps
```

`250 ms RTT + 0% loss` 下载：

```text
219.197914 Mbps
```

#### 当前分支 + `HY_RS_DISABLE_GSO=1`

`250 ms RTT + 30% loss` 下载：

```text
round 1: 72.303324 Mbps
round 2: 189.115724 Mbps
```

`250 ms RTT + 0% loss` 下载：

```text
147.607128 Mbps
```

### 当前判断

这说明：

- recovery gating 修正是有效的
- 它把 **0% loss** 场景明显拉高了
- 在 **30% loss** 下也有帮助，但还没回到历史最好 `278.347951 Mbps`
- `GSO off` 对高 loss 下载有时更好，但会拖低无丢包路径，因此当前还不适合作为默认


## 2026-03-20 - 继续冲 Quinn loss/recovery：针对性实验结果

这轮没有直接保留新的默认行为，但补了两个 **仅通过 env 打开的 BBR 实验开关**：

- `HY_RS_BBR_USE_RATE_BUCKET=1`
  - 让 BBR fallback 在 Quinn 里直接走 rate-token-bucket pacer，而不是继续走 Quinn 的 window pacer
- `HY_RS_BBR_ON_SENT_POST_FLIGHT=1`
  - 让 BBR `on_sent` 读取 Quinn 发送后的 `bytes_in_flight`，用于验证此前改成“发送前 inflight”是否是主要回归点

### 结果

#### 1. `HY_RS_BBR_USE_RATE_BUCKET=1`
- `250 ms RTT / 0% loss / 下载 / 5s`
  - `202.285073 Mbps`
- `250 ms RTT / 30% loss / 下载 / 10s x 2`
  - `79.451148 Mbps`
  - `178.719591 Mbps`

结论：
- 比当前主线略有波动，但**没有明显超过**此前的主线最好值；
- 说明单纯把 BBR fallback 切到 rate bucket，并不能把下载重新拉回 `278+ Mbps`。

#### 2. `HY_RS_BBR_ON_SENT_POST_FLIGHT=1`
- `250 ms RTT / 0% loss / 下载 / 5s`
  - `191.508203 Mbps`
- `250 ms RTT / 30% loss / 下载 / 10s x 2`
  - `40.246986 Mbps`
  - `157.436840 Mbps`
- 加上：
  - `HY_RS_PERSISTENT_CONGESTION_THRESHOLD=10`
  - `HY_RS_ADAPTIVE_GSO_ON_LOSS=0`
  后还出现 `download timed out`

结论：
- 把 BBR `on_sent` 改回“post-flight”**不是**主要修复方向，反而更差；
- 说明此前把 Quinn `on_sent` 映射到“发送前 inflight”的方向大概率是对的。

#### 3. 针对 `persistent congestion threshold` 和 adaptive GSO 的窄对比
在当前主线下，仍然使用大窗口：

```bash
HY_BENCH_STREAM_RWND=268435456
HY_BENCH_CONN_RWND=536870912
HY_SERVER_STREAM_RWND=268435456
HY_SERVER_CONN_RWND=536870912
```

对比：

- 默认：
  - `65.938483 Mbps`
  - `165.892051 Mbps`
- `HY_RS_ADAPTIVE_GSO_ON_LOSS=0 HY_RS_PERSISTENT_CONGESTION_THRESHOLD=10`
  - `62.147832 Mbps`
  - `205.642137 Mbps`

结论：
- 这组参数在这轮里**把 round 2 下载拉到了 `205.642137 Mbps`**；
- 但重复提升不够稳定，因此**没有直接写成默认值**；
- 说明 recovery 的主要矛盾仍然在，但“少做 adaptive GSO 干预 + 更晚进入 persistent congestion”在某些样本里确实更有利。

### 当前判断

这轮继续确认了三件事：

1. `BBR fallback -> rate bucket` 不是决定性突破口；
2. `on_sent` 的 pre-flight 语义不是当前最大回归点；
3. recovery 路径下，**adaptive GSO 干预时机** 和 **persistent congestion threshold** 仍然值得继续攻，但现在还没形成稳定超过历史最好 `278.347951 Mbps` 的默认方案。

补充：

- 为了继续攻这条路径，vendored `quinn-proto` 现在额外支持：
  - `HY_RS_ADAPTIVE_GSO_PACKET_THRESHOLD`
  - `HY_RS_ADAPTIVE_GSO_BYTES_THRESHOLD`
  - `HY_RS_ADAPTIVE_GSO_PERSISTENT_ONLY`
- 这些开关把 **adaptive GSO on loss 的触发条件** 参数化，便于继续做高 loss 场景复测。

## 2026-03-20 - adaptive GSO 默认改为仅在 persistent congestion 时触发

在 `250 ms RTT + 30%` 双向随机丢包的下载复测里，当前主拖累点已经比较明确：

- 默认 aggressive adaptive GSO：`26~41 Mbps`
- `HY_RS_ADAPTIVE_GSO_ON_LOSS=0`：`113~148 Mbps`
- `HY_RS_ADAPTIVE_GSO_PERSISTENT_ONLY=1`：`109~132 Mbps`
- `HY_RS_DISABLE_GSO=1`：`31~34 Mbps`

这说明问题不在于 “GSO 本身有害”，而在于：

- 当前 loss trigger 太激进；
- 对 **non-persistent random loss** 也会关 GSO，导致高 RTT / 高 loss 下载长期掉到低吞吐状态。

因此默认行为已改成：

- `HY_RS_ADAPTIVE_GSO_PERSISTENT_ONLY=1`

也就是：

- 保留 adaptive GSO on loss；
- 但默认**只在 persistent congestion 时**才关闭 segmentation offload；
- 如需恢复旧的更激进策略，可显式设置 `HY_RS_ADAPTIVE_GSO_PERSISTENT_ONLY=0`。

### 复测

仍使用：

```bash
HY_BENCH_STREAM_RWND=268435456
HY_BENCH_CONN_RWND=536870912
HY_SERVER_STREAM_RWND=268435456
HY_SERVER_CONN_RWND=536870912
```

#### 250 ms RTT / 0% loss / download / 5s x 2

- 默认：
  - `169.924214 Mbps`
  - `190.744751 Mbps`

#### 250 ms RTT / 30% loss / download / 10s x 2

- 默认（新的 persistent-only 默认）：
  - `107.272880 Mbps`
  - `101.047251 Mbps`
- 默认复跑一轮：
  - `113.867819 Mbps`
  - `117.038981 Mbps`
- 显式恢复旧 aggressive 行为 `HY_RS_ADAPTIVE_GSO_PERSISTENT_ONLY=0`：
  - `23.893450 Mbps`
  - `33.681424 Mbps`
- 默认 + `HY_RS_PERSISTENT_CONGESTION_THRESHOLD=10`：
  - `114.265188 Mbps`
  - `114.112491 Mbps`
- `HY_RS_ADAPTIVE_GSO_ON_LOSS=0 HY_RS_PERSISTENT_CONGESTION_THRESHOLD=10`：
  - `130.370213 Mbps`
  - `110.104462 Mbps`

### 结论

- 把默认值改成 **persistent-only** 后，`250 ms RTT + 30% loss` 下载默认吞吐已从此前的 `26~41 Mbps` 恢复到约 `100~117 Mbps`；
- 显式恢复旧 aggressive 行为时，结果会立刻掉回 `24~34 Mbps`，说明这次默认修正命中了当前主回归点；
- `persistent congestion threshold` 继续从 `5 -> 10` 的边际收益已经不大，说明下一阶段如果还要继续冲更高吞吐，重点应回到 BBR/recovery/sampler，而不是继续扩大 adaptive GSO 干预面。

## 2026-03-20 - BBR sampler 继续窄实验

这轮继续查 BBR sampler / recovery 主线，重点验证三个假设：

1. `app_limited` 判定是否过于激进；
2. `max_bandwidth` 的 round window 是否太短，导致高 RTT / 高 loss 下太快忘掉好样本；
3. ACK 稀疏是否会把 `ack_rate` 压低，进而拖低带宽样本。

### 代码证据

- `vendor/quinn-proto/src/congestion/bbr/mod.rs`
  - `on_ack_event()` 之前一直只用
    `prior_in_flight < target_cwnd`
    这条启发式去标记 sampler 为 `app_limited`
- 但 `AckEvent` 本身已经带了连接层 `app_limited`
  - `vendor/quinn-proto/src/congestion.rs`
  - `vendor/quinn-proto/src/connection/mod.rs`
- 因此先补了一个**默认关闭**的实验开关：
  - `HY_RS_BBR_USE_CONN_APP_LIMITED=1`
  - 启用后，BBR 改为直接使用连接层 `AckEvent.app_limited`
    来决定是否把 sampler 标成 `app_limited`

### 复测

仍使用：

```bash
HY_BENCH_STREAM_RWND=268435456
HY_BENCH_CONN_RWND=536870912
HY_SERVER_STREAM_RWND=268435456
HY_SERVER_CONN_RWND=536870912
```

场景：

- `250 ms RTT`
- `30% loss`
- download
- `10s x 2`

#### 当前默认

- `117.104278 Mbps`
- `128.789012 Mbps`

复跑：

- `100.386759 Mbps`
- `138.202054 Mbps`

#### 1) 连接层 app-limited

`HY_RS_BBR_USE_CONN_APP_LIMITED=1`

- `102.577963 Mbps`
- `107.730950 Mbps`

复跑：

- `116.342968 Mbps`
- `96.112327 Mbps`

补一个 `0% loss / 5s x 2`：

- `155.384039 Mbps`
- `208.712530 Mbps`

结论：

- 仅把 app-limited 判定改成连接层 `AckEvent.app_limited`，
  **没有稳定优于当前默认**；
- 所以 “BBR 自己的 inflight 启发式误判 app-limited” 这条线
  目前看**不是主矛盾**，至少不是单刀就能明显提速的主矛盾。

#### 2) 放大 `max_bandwidth` 窗口

`HY_RS_BBR_BW_WINDOW_ROUNDS=30`

- `109.903384 Mbps`
- `127.583021 Mbps`

`HY_RS_BBR_BW_WINDOW_ROUNDS=60`

- `114.331316 Mbps`
- `141.378965 Mbps`

复跑 `HY_RS_BBR_BW_WINDOW_ROUNDS=60`：

- `120.348254 Mbps`
- `130.987893 Mbps`

结论：

- 更大的 `max_bandwidth` window **有轻微正收益**；
- 说明“高 RTT / 高 loss 下太快忘掉好样本”这条假设**有一定成立**；
- 但收益仍不够大，说明它更像**次因**，不是决定性瓶颈。

#### 3) 更积极的 ACK frequency

`HY_RS_ACK_ENABLE=1 HY_RS_ACK_THRESH=1 HY_RS_ACK_MAX_DELAY_MS=1 HY_RS_ACK_REORDER_THRESHOLD=1`

- `115.704473 Mbps`
- `131.620625 Mbps`

复跑：

- `122.591912 Mbps`
- `60.057935 Mbps`

和 `HY_RS_BBR_BW_WINDOW_ROUNDS=60` 组合：

- `111.694149 Mbps`
- `136.077987 Mbps`

结论：

- 更积极的 ACK frequency **有时会帮助**，说明 ACK 形态确实会影响 sampler；
- 但波动很大，目前**不够稳定**，还不能作为默认修正。

### 当前判断

这轮更像是在收窄而不是直接拿到最终修复：

1. `HY_RS_BBR_USE_CONN_APP_LIMITED=1` 没有形成稳定收益；
2. `HY_RS_BBR_BW_WINDOW_ROUNDS=60` 有一定帮助，但不是决定性突破；
3. ACK frequency 会影响结果，但当前参数组合稳定性不够。

因此下一刀更应该继续回到 sampler 本体：

- `bandwidth = min(send_rate, ack_rate)` 的钳制是否过重；
- `send_rate` 参考锚点是否在高 loss / 重排序下变得过旧；
- 以及是否要把 `legacy ack-delta estimator` 更直接地接进 max_bw 更新逻辑，而不是只作为补充值。

## 2026-03-20 - sample 取值策略 / send-rate 锚点实验

这轮继续直接打 sampler 本体，主要做了两类**默认关闭**的实验开关：

- `HY_RS_BBR_SAMPLE_BANDWIDTH`
  - `min`（默认，当前逻辑）
  - `send`
  - `max`
- `HY_RS_BBR_SEND_RATE_ANCHOR`
  - `sent_state`（默认，当前逻辑）
  - `ack_event`

含义：

- `HY_RS_BBR_SAMPLE_BANDWIDTH`
  直接控制单包样本最后取：
  - `min(send_rate, ack_rate)`
  - `send_rate`
  - `max(send_rate, ack_rate)`
- `HY_RS_BBR_SEND_RATE_ANCHOR=ack_event`
  让 `send_rate` 优先使用**当前 ACK event 之前最近一次已确认包**作为参考锚点，
  而不是只用当前包在发送时保存下来的 `last_acked_packet_sent_time` /
  `total_bytes_sent_at_last_acked_packet`

### 验证

- `cargo test` ✅
- `cargo test -p hysteria-core --test runtime -- --ignored` ✅
- 临时拷贝 `vendor/quinn-proto` 单独跑新增解析/选择测试 ✅

### 复测

仍使用：

```bash
HY_BENCH_STREAM_RWND=268435456
HY_BENCH_CONN_RWND=536870912
HY_SERVER_STREAM_RWND=268435456
HY_SERVER_CONN_RWND=536870912
```

场景：

- `250 ms RTT`
- `30% loss`
- download
- `10s x 2`

#### 1) sample 取值策略

默认 `min`：

- `122.274425 Mbps`
- `111.494716 Mbps`
- `116.119745 Mbps`
- `122.621528 Mbps`

`HY_RS_BBR_SAMPLE_BANDWIDTH=send`：

- `103.126514 Mbps`
- `63.567996 Mbps`
- `106.996005 Mbps`
- `129.803964 Mbps`

`HY_RS_BBR_SAMPLE_BANDWIDTH=max`：

- `116.240744 Mbps`
- `123.315538 Mbps`
- `115.346072 Mbps`
- `50.977880 Mbps`

结论：

- 无论 `send` 还是 `max`，都**没有稳定优于默认 `min`**；
- `send` / `max` 都会引入更大的波动；
- 说明现在直接放松 `min(send_rate, ack_rate)` 这条硬钳制，
  **不是一个足够稳的修复方向**。

#### 2) send-rate 锚点

默认 `HY_RS_BBR_SEND_RATE_ANCHOR=sent_state`：

- `148.711975 Mbps`
- `150.481188 Mbps`
- `124.464907 Mbps`
  - 另一轮 round 1 出现了 `0.015564 Mbps` 的异常样本

`HY_RS_BBR_SEND_RATE_ANCHOR=ack_event`：

- `131.689817 Mbps`
- `179.329274 Mbps`
- `145.938671 Mbps`
- `163.223179 Mbps`

补测：

- `HY_RS_BBR_SEND_RATE_ANCHOR=ack_event HY_RS_BBR_BW_WINDOW_ROUNDS=60`
  - `126.724782 Mbps`
  - `93.824236 Mbps`
- `HY_RS_BBR_SEND_RATE_ANCHOR=ack_event`，`0% loss / 5s x 2`
  - `181.809052 Mbps`
  - `206.072040 Mbps`

结论：

- `ack_event` 锚点相比当前默认，**更像是有轻微正收益**；
- 特别是 `30% loss` 下，round 2 样本多次高于当前默认；
- 但收益还**不够稳定，也不够大**，暂时还不足以直接改默认；
- 和 `BW_WINDOW_ROUNDS=60` 叠加后没有形成更稳的收益。

### 当前判断

这轮把 sampler 主线再缩小了一步：

1. 直接把样本从 `min` 改成 `send` / `max`，不值得；
2. `send_rate` 锚点“过旧”这条线**比 sample 公式本身更像真问题**；
3. 但只改锚点仍然不够，下一步更可能要继续看：
   - `legacy ack-delta estimator` 怎样更直接参与 `max_bw` 更新；
   - 或者让 `send_rate` / `ack_rate` 的选择不再只是一次性的单包取值，而是更偏向 ACK-event 级别的融合。

## 2026-03-20 - legacy estimator / ACK-event 融合实验

继续沿着上一轮的判断，直接试了一刀 **ACK-event 级别融合**：

- 新增环境变量：
  - `HY_RS_BBR_ACK_EVENT_BW_FUSION`
- 当前支持：
  - `off`（默认，保持当前行为）
  - `send_cap`

`send_cap` 的含义：

- 仍然使用 legacy ack-delta estimator；
- 但在 ACK-event 级别，把 legacy 带宽样本改成：
  - `min(legacy_bandwidth, max_send_rate)`
- 目的是减少单纯 ack-delta estimator 因 ACK compression 带来的过估，同时让 ACK-event 融合更贴近当前发送速率。

实现点：

- `vendor/quinn-proto/src/congestion/bbr/bw_estimation.rs`
  - 新增 `AckEventBandwidthFusionStrategy`
  - `BandwidthSampler::on_congestion_event(...)` 现在会接收 `event_app_limited`
  - 在 `send_cap` 模式下，ACK-event 融合样本会用 `event.app_limited` 作为 app-limited 归类
- `vendor/quinn-proto/src/congestion/bbr/mod.rs`
  - 调用 sampler 时把 `event.app_limited` 传进去

### 验证

- `cargo test` ✅
- `cargo test -p hysteria-core --test runtime -- --ignored` ✅
- 临时拷贝 `vendor/quinn-proto` 单独跑新增 `ack_event_bandwidth_fusion_strategy` 测试 ✅

### 复测

仍使用：

```bash
HY_BENCH_STREAM_RWND=268435456
HY_BENCH_CONN_RWND=536870912
HY_SERVER_STREAM_RWND=268435456
HY_SERVER_CONN_RWND=536870912
```

场景：

- `250 ms RTT`
- `30% loss`
- download
- `10s x 2`

#### 默认 `fusion=off`

- `106.291084 Mbps`
- `121.860369 Mbps`
- `120.101709 Mbps`
- `98.007498 Mbps`

#### `HY_RS_BBR_ACK_EVENT_BW_FUSION=send_cap`

- `103.393242 Mbps`
- `123.490339 Mbps`
- `118.153231 Mbps`
- `130.526904 Mbps`

#### `send_cap + HY_RS_BBR_SEND_RATE_ANCHOR=ack_event`

- `107.974016 Mbps`
- `108.946058 Mbps`

#### `send_cap` 在 `0% loss / 5s x 2`

- `149.093391 Mbps`
- `215.563341 Mbps`

### 结论

- `ACK-event fusion=send_cap` **没有明显稳定优于默认路径**；
- 它不会立刻把结果打坏，但也没有形成足够强的正收益；
- 和 `HY_RS_BBR_SEND_RATE_ANCHOR=ack_event` 叠加后也没有出现更稳的改善。

### 当前判断

这轮再次说明：

1. `legacy estimator / ACK-event` 这条线**确实相关**；
2. 但“只做一层 event-level send-cap 融合”还不够；
3. 更可能的下一步是：
   - 直接调整 `max_bw` 更新逻辑，而不只是 sampler 内部先做 sample；
   - 或者进一步看 ACK/loss batch 与 BBR round / startup gating 的联动。

## 2026-03-20 - max_bw 更新逻辑实验

这轮继续沿着上一轮的判断，直接下到：

- `vendor/quinn-proto/src/congestion/bbr/mod.rs`
  - `self.max_bandwidth.update_max(...)`

当前默认逻辑是：

- `sample.bytes_acked > 0`
- 并且：
  - `!sample.sample_is_app_limited`
  - 或 `sample.sample_max_bandwidth > current_bw`

才会更新 rolling `max_bw` window。

为了验证“高 loss 下可能因为 app-limited gating 把好样本窗口忘掉太快”，新增了一个**默认关闭**的实验开关：

- `HY_RS_BBR_MAX_BW_UPDATE=refresh_current`

它的行为是：

- 如果当前 ACK batch 被判成 `app_limited`
- 且没有带来更高的 `sample_max_bandwidth`
- 那么不再完全跳过更新，而是用当前 `bandwidth_estimate()` 去 **refresh rolling window**

也就是：

- 默认：app-limited + no-new-high-sample -> 不更新 `max_bw`
- `refresh_current`：app-limited + no-new-high-sample -> 用当前 `max_bw` 刷新时间窗口

### 验证

- `cargo test` ✅
- `cargo test -p hysteria-core --test runtime -- --ignored` ✅
- 临时拷贝 `vendor/quinn-proto` 单独跑新增 `max_bw_update_strategy` 测试 ✅

### 复测

仍使用：

```bash
HY_BENCH_STREAM_RWND=268435456
HY_BENCH_CONN_RWND=536870912
HY_SERVER_STREAM_RWND=268435456
HY_SERVER_CONN_RWND=536870912
```

场景：

- `250 ms RTT`
- `30% loss`
- download
- `10s x 2`

#### 默认

- `103.238221 Mbps`
- `122.031165 Mbps`
- `103.884420 Mbps`
- `49.680832 Mbps`

#### `HY_RS_BBR_MAX_BW_UPDATE=refresh_current`

- `112.621986 Mbps`
- `99.325857 Mbps`
- `111.941193 Mbps`
- `152.530625 Mbps`

复跑：

- `118.979652 Mbps`
- `132.041368 Mbps`

#### `refresh_current + HY_RS_BBR_BW_WINDOW_ROUNDS=60`

- `111.925926 Mbps`
- `108.197005 Mbps`

#### `refresh_current + HY_RS_BBR_SEND_RATE_ANCHOR=ack_event`

- `100.398306 Mbps`
- `139.816009 Mbps`

#### `refresh_current` 在 `0% loss / 5s x 2`

- `191.830591 Mbps`
- `186.327205 Mbps`

### 结论

- `refresh_current` 这刀相比前面几轮实验，**更像是有一定实际收益**；
- 它没有把结果稳定推到特别高，但确实多次把 `30% loss` 下载拉到：
  - `132 Mbps`
  - `139.8 Mbps`
  - `152.5 Mbps`
- 同时没有观察到 `0% loss` 明显副作用。

但目前它仍然有两个问题：

1. 波动还是明显；
2. 和 `BW_WINDOW_ROUNDS=60` / `SEND_RATE_ANCHOR=ack_event` 叠加后没有形成非常稳的进一步提升。

### 当前判断

截至这一轮：

- 直接改 sample 公式：不值得；
- 只改 ACK-event fusion：不够；
- **max_bw rolling-window refresh** 比前两刀更像正方向；

所以下一步如果还继续攻这条线，更合理的是：

1. 继续围绕 `max_bw` 更新策略做更细 gating；
2. 或者直接转去看：
   - ACK/loss batch 与 round 计数的关系
   - `check_if_full_bw_reached()` / startup exit 与 `max_bw` 的联动。

## 2026-03-20 - startup exit / round gating 实验

这轮继续沿着上一轮结论，直接去看：

- `check_if_full_bw_reached()` 前面的 startup gate
- `is_round_start` 的 round 推进条件

新增/补齐了两个**默认关闭**的实验开关：

- `HY_RS_BBR_STARTUP_FULL_BW_GATE=ignore_app_limited`
  - 不再因为 `last_sample_is_app_limited` 直接跳过 `check_if_full_bw_reached()`
- `HY_RS_BBR_ROUND_GATING=largest_observed`
  - `is_round_start` 不只看最大 acked 包号，也把同一 ACK batch 中最大的 lost 包号纳入

### 验证

- `cargo test` ✅
- `cargo test -p hysteria-core --test runtime -- --ignored` ✅
- 临时拷贝 `vendor/quinn-proto` 单独跑：
  - `startup_full_bw_gate_strategy` ✅
  - `round_gating_strategy` ✅

### 复测

仍使用：

```bash
HY_BENCH_STREAM_RWND=268435456
HY_BENCH_CONN_RWND=536870912
HY_SERVER_STREAM_RWND=268435456
HY_SERVER_CONN_RWND=536870912
```

场景：

- `250 ms RTT`
- `30% loss`
- download
- `10s x 2`

#### baseline

- `117.931745 Mbps`
- `116.624738 Mbps`

复跑：

- `122.720543 Mbps`
- `106.227891 Mbps`

#### `HY_RS_BBR_STARTUP_FULL_BW_GATE=ignore_app_limited`

- `120.240395 Mbps`
- `130.173822 Mbps`

复跑：

- `104.773842 Mbps`
- `131.514605 Mbps`

#### `HY_RS_BBR_ROUND_GATING=largest_observed`

- `113.987943 Mbps`
- `94.747023 Mbps`

#### `ignore_app_limited + largest_observed`

- `120.493751 Mbps`
- `126.460435 Mbps`

复跑：

- `107.561024 Mbps`
- `124.619390 Mbps`

#### `HY_RS_BBR_MAX_BW_UPDATE=refresh_current`

- `112.341687 Mbps`
- `127.625512 Mbps`

#### `ignore_app_limited + refresh_current`

- `113.565857 Mbps`
- `107.719985 Mbps`

#### `largest_observed + refresh_current`

- `113.572677 Mbps`
- `120.217476 Mbps`

#### `ignore_app_limited + largest_observed + refresh_current`

- `114.078129 Mbps`
- `143.436421 Mbps`

复跑：

- `41.056701 Mbps`
- `127.134268 Mbps`

#### `ignore_app_limited + largest_observed + refresh_current` 在 `0% loss / 5s x 2`

- `195.326916 Mbps`
- `210.226966 Mbps`

### 结论

- 单看这轮，**`startup_full_bw_gate=ignore_app_limited` 比 `round_gating=largest_observed` 更像正方向**；
- `largest_observed` 单独开收益不稳，甚至比 baseline 更差；
- 三者组合有过一次 `143.4 Mbps` 的高点，但复跑里仍有一次掉到 `41 Mbps`，说明**波动仍然很大**；
- 目前还不能把 startup/round gating 判成新的主突破口。

### 当前判断

截至这轮：

1. `ignore_app_limited` 值得保留为实验旋钮；
2. `largest_observed` 还不够稳，不值得改默认；
3. startup/round gating 更像是**次级联动项**，要和 `max_bw` refresh 一起看，但单靠这一刀还不够。

## 2026-03-20 - max_bw + app-limited gating 联动复测

这轮回到 `max_bw` 与 `app_limited` 的联动本身，直接复测现有实验旋钮：

- `HY_RS_BBR_MAX_BW_APP_LIMITED=prefer_non_app_limited`

它的行为是：

- 如果一个 ACK batch 里，最高 `sample_max_bandwidth` 来自 `app_limited` sample；
- 但同一批里也存在 `non-app-limited` sample；
- 那么优先退回到这批里的最高 `non-app-limited` sample 去参与 `max_bw` 更新。

### 验证

- workspace `cargo test`：本轮未改逻辑代码，沿用上一轮通过结果 ✅
- 单独 vendor 测试：
  - `max_bw_app_limited_strategy` ✅

### 复测

仍使用：

```bash
HY_BENCH_STREAM_RWND=268435456
HY_BENCH_CONN_RWND=536870912
HY_SERVER_STREAM_RWND=268435456
HY_SERVER_CONN_RWND=536870912
```

场景：

- `250 ms RTT`
- `30% loss`
- download
- `10s x 2`

#### baseline

- `105.389744 Mbps`
- `96.980857 Mbps`

#### `HY_RS_BBR_MAX_BW_APP_LIMITED=prefer_non_app_limited`

一次中途超时：

- `113.076248 Mbps`
- round 2 timeout

重跑：

- `109.582538 Mbps`
- `103.742119 Mbps`

#### `prefer_non_app_limited + HY_RS_BBR_MAX_BW_UPDATE=refresh_current`

- `113.486083 Mbps`
- `122.864629 Mbps`

补测：

- `119.500634 Mbps`
- round 2 timeout

#### `prefer_non_app_limited + HY_RS_BBR_STARTUP_FULL_BW_GATE=ignore_app_limited`

- `115.919738 Mbps`
- `119.940057 Mbps`

补测：

- `107.244288 Mbps`
- `27.777937 Mbps`

#### `prefer_non_app_limited + ignore_app_limited + refresh_current`

- `99.364903 Mbps`
- `129.350800 Mbps`

#### `prefer_non_app_limited` 在 `0% loss / 5s x 2`

- `156.188508 Mbps`
- `181.636142 Mbps`

### 对照观察

和前面已经测过的结果相比：

- `ignore_app_limited` 单独开，曾有：
  - `120.240395 Mbps`
  - `130.173822 Mbps`
- `refresh_current` 单独开，曾有：
  - `118.979652 Mbps`
  - `132.041368 Mbps`
- `ignore_app_limited + refresh_current` 也曾有：
  - `119.212603 Mbps`
  - `134.953216 Mbps`

当前这轮里，`prefer_non_app_limited`：

- 有一点正收益信号；
- 但没有稳定优于 `refresh_current` 或 `ignore_app_limited`；
- 还伴随了明显波动与 timeout。

再补一轮更近的同批对照：

- `ignore_app_limited`
  - `104.622969 Mbps`
  - `139.438000 Mbps`
- `ignore_app_limited + refresh_current`
  - `119.212603 Mbps`
  - `134.953216 Mbps`
- `prefer_non_app_limited`
  - `119.500634 Mbps`
  - `75.154784 Mbps`
- `prefer_non_app_limited + ignore_app_limited`
  - `107.244288 Mbps`
  - `27.777937 Mbps`
- `prefer_non_app_limited + refresh_current`
  - round 1 timeout

这批近邻对照进一步说明：

- `prefer_non_app_limited` 不是完全没收益；
- 但它的**稳定性明显差于** `ignore_app_limited + refresh_current`；
- 目前更像是带来额外波动，而不是形成稳定增益。

### 结论

- `prefer_non_app_limited` 这条线**方向上合理，但目前不够稳**；
- 它不像主突破口，更像对 `app_limited` 误伤的一层温和修补；
- 当前最值得继续保留观察的，仍然是：
  1. `HY_RS_BBR_MAX_BW_UPDATE=refresh_current`
  2. `HY_RS_BBR_STARTUP_FULL_BW_GATE=ignore_app_limited`

### 当前判断

截至这轮：

1. `max_bw + app_limited gating` 确实相关；
2. 但“优先同批 non-app-limited sample” 这一刀收益有限；
3. 下一步更像应该继续看：
   - `sample.sample_is_app_limited` 与 `last_sample_is_app_limited` 的错位；
   - 是否要引入 **event-level app-limited gate**，而不是只看单个 winning sample。

## 2026-03-20 - event-level app-limited gate 实验

这轮继续沿着上一轮结论，给：

- `HY_RS_BBR_MAX_BW_APP_LIMITED`

新增了一个更贴近问题本体的模式：

- `event_non_app_limited_ok`

行为是：

- 如果 ACK batch 里的 winning sample 是 `app_limited`；
- 但同一批里**存在任意 non-app-limited sample**；
- 则仍然保留 winning `sample_max_bandwidth`，只是把 `sample_is_app_limited` 改成 `false`，让这批样本可以继续参与 `max_bw` 更新。

和上一轮 `prefer_non_app_limited` 的区别：

- `prefer_non_app_limited`：回退到同批里的最高 non-app-limited sample
- `event_non_app_limited_ok`：**不降 sample 值**，只放松 app-limited gate

### 验证

- `cargo test` ✅
- `cargo test -p hysteria-core --test runtime -- --ignored` ✅
- 临时拷贝 `vendor/quinn-proto` 单独跑：
  - `max_bw_app_limited_strategy` ✅

### 复测

仍使用：

```bash
HY_BENCH_STREAM_RWND=268435456
HY_BENCH_CONN_RWND=536870912
HY_SERVER_STREAM_RWND=268435456
HY_SERVER_CONN_RWND=536870912
```

场景：

- `250 ms RTT`
- `30% loss`
- download
- `10s x 2`

#### baseline

- `97.748887 Mbps`
- `100.096986 Mbps`

#### `ignore_app_limited + refresh_current` 对照

- `106.476800 Mbps`
- `111.380586 Mbps`

复跑：

- `116.911735 Mbps`
- `131.575855 Mbps`

#### `HY_RS_BBR_MAX_BW_APP_LIMITED=event_non_app_limited_ok`

- `120.209205 Mbps`
- `87.300727 Mbps`

复跑：

- `128.228887 Mbps`
- `119.789353 Mbps`

#### `event_non_app_limited_ok + refresh_current`

- `115.702344 Mbps`
- `106.427775 Mbps`

#### `event_non_app_limited_ok + ignore_app_limited`

- `107.278185 Mbps`
- `122.356379 Mbps`

#### `event_non_app_limited_ok + ignore_app_limited + refresh_current`

- `119.509610 Mbps`
- `122.145785 Mbps`

复跑：

- `98.598453 Mbps`
- `113.125535 Mbps`

#### `event_non_app_limited_ok` 在 `0% loss / 5s x 2`

- `163.730624 Mbps`
- `211.615576 Mbps`

#### `event_non_app_limited_ok + ignore_app_limited + refresh_current` 在 `0% loss / 5s x 2`

- `171.877011 Mbps`
- `216.329282 Mbps`

### 结论

- 这轮看下来，**`event_non_app_limited_ok` 比 `prefer_non_app_limited` 更像正方向**；
- 它的关键好处是：
  - 不丢掉 winning sample 的带宽值；
  - 只是在 ACK-event 级别放松 app-limited gate；
- 目前看，**单独开 `event_non_app_limited_ok` 就已经有一定收益信号**；
- 反而继续叠 `ignore_app_limited + refresh_current` 后，没有明显更稳。

### 当前判断

截至这一轮：

1. `sample.sample_is_app_limited` 这层 winning-sample 级别门控，确实偏激进；
2. 改成 **event-level gate** 后，结果比上一轮 “回退到 non-app sample” 更自然；
3. 下一步更合理的是：
   - 再补一轮更密一点的复跑；
   - 如果趋势保持，就考虑把 `event_non_app_limited_ok` 作为新的默认候选继续验证。

## 2026-03-20 - `event_non_app_limited_ok` 默认候选验证

上一轮里，`event_non_app_limited_ok` 作为实验旋钮看起来比 `prefer_non_app_limited` 更自然，所以这轮专门做了：

- “新默认候选” vs “显式 legacy”

对照。

### 验证

- `cargo test` ✅
- `cargo test -p hysteria-core --test runtime -- --ignored` ✅
- 临时拷贝 `vendor/quinn-proto` 单独跑：
  - `max_bw_app_limited_strategy` ✅

### 复测

仍使用：

```bash
HY_BENCH_STREAM_RWND=268435456
HY_BENCH_CONN_RWND=536870912
HY_SERVER_STREAM_RWND=268435456
HY_SERVER_CONN_RWND=536870912
```

场景：

- `250 ms RTT`
- `30% loss`
- download
- `10s x 2`

#### 新默认候选（`event_non_app_limited_ok`）

- `115.457488 Mbps`
- `82.749236 Mbps`

复跑：

- `105.865279 Mbps`
- `132.759473 Mbps`

#### 显式 legacy

- `117.901028 Mbps`
- `121.196800 Mbps`

复跑：

- `120.558031 Mbps`
- `114.132196 Mbps`

#### 新默认候选 + `ignore_app_limited + refresh_current`

- `40.365324 Mbps`
- `141.843044 Mbps`

复跑：

- `118.861438 Mbps`
- `142.087326 Mbps`

#### legacy + `ignore_app_limited + refresh_current`

- `95.891238 Mbps`
- `122.348540 Mbps`

复跑：

- `108.642576 Mbps`
- `123.547419 Mbps`

#### 新默认候选在 `0% loss / 5s x 2`

- `164.374244 Mbps`
- `215.917164 Mbps`

### 结论

- 这轮数据**还不支持直接把 `event_non_app_limited_ok` 改成默认**；
- plain default 对照里，它没有稳定优于 legacy；
- 和 `ignore_app_limited + refresh_current` 叠加时，虽然有更高高点，但波动也更大；
- 所以当前更稳妥的结论是：
  - **保留 `event_non_app_limited_ok` 作为实验旋钮**
  - **默认行为先不改**

### 当前判断

截至这轮：

1. event-level gate 是个值得保留的方向；
2. 但证据还不够强，不足以直接进默认；
3. 下一步更适合继续：
   - 复跑更密的矩阵；
   - 或转去看 `app_limited` 判定来源本身，而不是只改 `max_bw` gate。

## 2026-03-20 - app_limited 来源与 heuristic 阈值复测

这轮继续往下查 `app_limited` 来源本身，先做了代码勘察，再补了一个**默认不变**的小实验开关：

- `HY_RS_BBR_APP_LIMITED_TARGET_PCT`

作用位置：

- `vendor/quinn-proto/src/congestion/bbr/mod.rs`
  - 默认 heuristic 还是：
    - `bytes_in_flight < target_cwnd * 100%`
  - 但现在可改成：
    - `bytes_in_flight < target_cwnd * N%`

也就是：

- `100`：当前行为
- `75`：更保守
- `50`：更保守

### 代码勘察结论

当前有两条主要来源：

1. **BBR 默认 heuristic**
   - `bytes_in_flight < target_cwnd`
   - 在高 RTT / 高 loss 下容易因为 inflight 暂时下滑而误判
2. **connection 层 `AckEvent.app_limited`**
   - 来自 `Connection.app_limited`
   - 本质上是上次 `poll_transmit()` 是否“没数据可发且不是 congestion/pacing 挡住”的粗粒度 latch

从代码语义看，更可疑的还是：

- **默认 inflight heuristic 太激进**

### 验证

- `cargo test` ✅
- `cargo test -p hysteria-core --test runtime -- --ignored` ✅
- 临时拷贝 `vendor/quinn-proto` 单独跑：
  - `app_limited_target_pct` ✅
  - `max_bw_app_limited_strategy` ✅

### 复测

仍使用：

```bash
HY_BENCH_STREAM_RWND=268435456
HY_BENCH_CONN_RWND=536870912
HY_SERVER_STREAM_RWND=268435456
HY_SERVER_CONN_RWND=536870912
```

场景：

- `250 ms RTT`
- `30% loss`
- download
- `10s x 2`

#### baseline

- `117.523875 Mbps`
- `130.392142 Mbps`

复跑：

- `121.738324 Mbps`
- `116.706355 Mbps`

#### `HY_RS_BBR_APP_LIMITED_TARGET_PCT=75`

- `105.323138 Mbps`
- `131.660017 Mbps`

#### `HY_RS_BBR_APP_LIMITED_TARGET_PCT=50`

- `116.157680 Mbps`
- `119.982883 Mbps`

复跑：

- `115.130272 Mbps`
- `133.987190 Mbps`

#### `HY_RS_BBR_USE_CONN_APP_LIMITED=1`

- `120.471290 Mbps`
- `138.748987 Mbps`

复跑：

- `120.557307 Mbps`
- `138.797265 Mbps`

#### `HY_RS_BBR_USE_CONN_APP_LIMITED=1 + ignore_app_limited + refresh_current`

- `97.074151 Mbps`
- `125.116244 Mbps`

#### `HY_RS_BBR_USE_CONN_APP_LIMITED=1` 在 `0% loss / 5s x 2`

- `189.814849 Mbps`
- `206.955859 Mbps`

### 结论

- 这轮里，**`HY_RS_BBR_USE_CONN_APP_LIMITED=1` 反而是最稳定、最像正收益的一组**；
- `TARGET_PCT=50` 也比 baseline 更像正方向，但不如 `conn_only` 稳；
- `TARGET_PCT=75` 没看出明显优势；
- 说明问题确实很可能在：
  - **默认 inflight heuristic 过于激进**

### 当前判断

截至这轮：

1. `app_limited` 来源本身确实是当前更值得打的线；
2. 纯 connection source 在当前代码状态下，表现比之前更好；
3. 下一步更合理的是：
   - 直接把 `HY_RS_BBR_USE_CONN_APP_LIMITED=1` 作为新的默认候选做复测；
   - 或继续做一个更温和的 hybrid，而不是完全沿用当前 `inflight < target_cwnd`。

## 2026-03-20 - `HY_RS_BBR_USE_CONN_APP_LIMITED=1` 默认候选验证

上一轮里，`HY_RS_BBR_USE_CONN_APP_LIMITED=1` 看起来是最稳的一组，所以这轮专门做了：

- 新默认候选（connection source）
- 显式 legacy heuristic（`HY_RS_BBR_USE_CONN_APP_LIMITED=0`）

对照。

### 验证

- `cargo test` ✅
- `cargo test -p hysteria-core --test runtime -- --ignored` ✅
- 临时拷贝 `vendor/quinn-proto` 单独跑：
  - `connection_app_limited_value` ✅
  - `app_limited_target_pct` ✅
  - `max_bw_app_limited_strategy` ✅

### 复测

仍使用：

```bash
HY_BENCH_STREAM_RWND=268435456
HY_BENCH_CONN_RWND=536870912
HY_SERVER_STREAM_RWND=268435456
HY_SERVER_CONN_RWND=536870912
```

场景：

- `250 ms RTT`
- `30% loss`
- download
- `10s x 2`

#### 新默认候选（connection source）

- `105.649659 Mbps`
- `62.165505 Mbps`

复跑：

- `112.628449 Mbps`
- `129.672000 Mbps`

#### 显式 legacy heuristic

- `126.425715 Mbps`
- `127.006042 Mbps`

复跑：

- `108.717809 Mbps`
- `145.777681 Mbps`

#### 新默认候选 + `ignore_app_limited + refresh_current`

- `115.908718 Mbps`
- `114.980406 Mbps`

复跑：

- `112.249821 Mbps`
- `138.250910 Mbps`

#### legacy heuristic + `ignore_app_limited + refresh_current`

- `121.682421 Mbps`
- `124.444372 Mbps`

复跑：

- `116.985225 Mbps`
- `106.052316 Mbps`

#### 新默认候选在 `0% loss / 5s x 2`

- `183.306926 Mbps`
- `220.211367 Mbps`

### 结论

- 这轮数据**还不支持直接把 connection source 改成默认**；
- 它仍然有正收益信号，但 plain default 对照里波动太大；
- 和 legacy heuristic 相比，没有形成足够稳定的优势；
- 所以当前更稳妥的结论是：
  - **保留 `HY_RS_BBR_USE_CONN_APP_LIMITED=1` 作为实验旋钮**
  - **默认行为先不改**

### 当前判断

截至这轮：

1. `app_limited` 来源仍然是值得继续打的线；
2. 但 “直接切 connection source 为默认” 的证据还不够强；
3. 下一步更合理的是：
   - 做一个更温和的 hybrid；
   - 或继续把 `TARGET_PCT=50` 与其他旋钮联动复跑。

## 2026-03-20 - `HY_RS_BBR_APP_LIMITED_SOURCE=hybrid` 复测

这轮继续沿着 `app_limited` 来源这条线往下打，但不再直接在

- `heuristic`
- `connection`

之间二选一，而是补了一个更温和的实验模式：

- `HY_RS_BBR_APP_LIMITED_SOURCE=hybrid`

语义是：

- 只有 **connection source** 和 **inflight heuristic** 同时都认为 `app_limited`
- 才调用 `sampler.on_app_limited()`

默认行为仍保持不变：

- `HY_RS_BBR_APP_LIMITED_SOURCE` 默认 `heuristic`
- 旧旋钮 `HY_RS_BBR_USE_CONN_APP_LIMITED=1` 仍兼容，等价于显式 `connection`

### 验证

- `cargo test` ✅
- `cargo test -p hysteria-core --test runtime -- --ignored` ✅
- 临时拷贝 `vendor/quinn-proto` 单独跑：
  - `app_limited_source` ✅
  - `connection_app_limited_value` ✅
  - `app_limited_target_pct` ✅

### 复测

仍使用：

```bash
HY_BENCH_STREAM_RWND=268435456
HY_BENCH_CONN_RWND=536870912
HY_SERVER_STREAM_RWND=268435456
HY_SERVER_CONN_RWND=536870912
```

场景：

- `250 ms RTT`
- `30% loss`
- download
- `10s x 2`

#### baseline

- `122.938160 Mbps`
- `134.898247 Mbps`

复跑：

- `115.395946 Mbps`
- `123.272966 Mbps`

#### `HY_RS_BBR_APP_LIMITED_SOURCE=hybrid`

- `109.243005 Mbps`
- `116.071661 Mbps`

#### `HY_RS_BBR_APP_LIMITED_SOURCE=hybrid HY_RS_BBR_APP_LIMITED_TARGET_PCT=50`

- `127.433654 Mbps`
- `124.033608 Mbps`

复跑：

- `135.781448 Mbps`
- `63.471112 Mbps`

#### `HY_RS_BBR_APP_LIMITED_SOURCE=connection`

- `113.097168 Mbps`
- `123.298397 Mbps`

#### `HY_RS_BBR_APP_LIMITED_SOURCE=hybrid + ignore_app_limited + refresh_current`

- `108.988096 Mbps`
- `111.804659 Mbps`

#### `HY_RS_BBR_APP_LIMITED_SOURCE=hybrid HY_RS_BBR_APP_LIMITED_TARGET_PCT=50 + ignore_app_limited + refresh_current`

- `70.347542 Mbps`
- `126.540752 Mbps`

#### `HY_RS_BBR_APP_LIMITED_SOURCE=hybrid HY_RS_BBR_APP_LIMITED_TARGET_PCT=50` 在 `0% loss / 5s x 2`

- `164.391487 Mbps`
- `215.157181 Mbps`

### 结论

- **plain `hybrid` 本身没有优于 baseline**
- **`hybrid + TARGET_PCT=50`** 是这轮里最像有上探空间的一组
- 但它也出现了明显低样本（`63.471112 Mbps`），所以**还不够稳**
- 和 `ignore_app_limited + refresh_current` 叠加后，结果反而更散

所以截至这轮：

1. `app_limited` 来源这条线仍然有价值；
2. 但 `hybrid` 默认候选证据还不够；
3. 当前更像值得继续保留的实验组合是：
   - `HY_RS_BBR_APP_LIMITED_SOURCE=hybrid`
   - `HY_RS_BBR_APP_LIMITED_TARGET_PCT=50`
4. 默认行为先继续保持：
   - `heuristic`

## 2026-03-20 - `app_limited` trace 诊断

为了继续查清：

- 为什么 `HY_RS_BBR_APP_LIMITED_SOURCE=hybrid HY_RS_BBR_APP_LIMITED_TARGET_PCT=50`
- 还会偶发出现单轮明显低样本

这轮加了两个**默认关闭**的诊断开关：

- `HY_RS_BBR_APP_LIMITED_TRACE=1`
- `HY_RS_BBR_APP_LIMITED_TRACE_EVERY=<N>`

trace 位置在：

- `vendor/quinn-proto/src/congestion/bbr/mod.rs`
  - `maybe_app_limited`
  - `sampler.on_app_limited()`
  - `on_ack_event`

日志前缀：

- `BBR_APP_LIMITED_DECISION`
- `BBR_APP_LIMITED_TRIGGER`
- `BBR_APP_LIMITED_SAMPLE`

### 验证

- `cargo test` ✅
- `cargo test -p hysteria-core --test runtime -- --ignored` ✅
- 临时拷贝 `vendor/quinn-proto` 单独跑：
  - `app_limited_trace_every` ✅
  - `app_limited_source` ✅

### 诊断复测

这轮为了抓 trace，使用本地 netns harness 跑了：

- `250 ms RTT`
- `30% loss`
- `download`
- `10s x 4`
- `HY_RS_BBR_APP_LIMITED_SOURCE=hybrid`
- `HY_RS_BBR_APP_LIMITED_TARGET_PCT=50`
- `HY_RS_BBR_APP_LIMITED_TRACE=1`
- `HY_RS_BBR_APP_LIMITED_TRACE_EVERY=128`

> 注意：这轮 trace 非常重，吞吐值本身主要用于诊断，不宜直接和无 trace 的 benchmark 对比。

bench 摘要：

- `83.689896 Mbps`
- `99.200869 Mbps`
- `95.194528 Mbps`
- `101.485188 Mbps`

日志：

- `/tmp/hy_trace_app_limited_1774042488/server.log`
- `/tmp/hy_trace_app_limited_1774042488/bench.log`

### 诊断结论

这轮 trace 已经把主要问题缩得比较清楚：

1. `hybrid+pct50` 仍然会在**新一轮业务突发开始时**触发一小簇 `selected_app_limited=true`
2. 触发条件通常同时满足：
   - `conn_app_limited=true`
   - `heuristic_app_limited=true`
3. 触发后 `sampler.on_app_limited()` 会把 sampler 带进一段 **app-limited sample 连续区**
4. 在这段区间里，trace 能看到大量：
   - `sample_is_app_limited=true`
   - `sample_max_bandwidth_non_app_limited=0`
   - `measurement=0`

也就是：

- **新一轮开始前后的 app-limited gap**
- 会让 sampler 连续产出一段无法有效刷新 `max_bw` 的样本
- 如果这段恢复期在某一轮里拉得更长，就会把那一轮 10s 下载均值明显拉低

### 直接证据

例如在 `server.log` 的 `ack_event=10350..10440` 附近，可以看到：

- 一小簇 `BBR_APP_LIMITED_DECISION ... selected=true`
- 紧接着一串 `BBR_APP_LIMITED_TRIGGER`
- 然后连续很多条：
  - `sample_is_app_limited=true`
  - `measurement=0`
  - `sample_max_bandwidth_non_app_limited=0`

这说明：

- 真正的问题不只是“有没有触发一次 app_limited”
- 而是 **一旦在 round 边界/stream 边界误触发，后面会拖出一段持续的 app-limited sample 尾巴**

### 当前判断

截至这轮，更合理的下一步已经比较明确：

1. 继续保留 `hybrid+pct50` 作为候选；
2. 但不能只改 `maybe_app_limited` 入口；
3. 下一刀应该直接打：
   - **sampler 的 app-limited 退出/恢复条件**
   - 或 **stream/round 边界上的 app-limited debounce**

## 2026-03-20 - `app_limited` debounce 实验

为了抑制：

- round / stream 边界上的重复 `sampler.on_app_limited()` 触发
- 以及由此拖长的 app-limited sample 尾巴

这轮加了一个**默认关闭**的实验开关：

- `HY_RS_BBR_APP_LIMITED_DEBOUNCE=1`

行为是：

- 当 `maybe_app_limited` 再次命中
- 但 sampler 已经处于 app-limited phase
- 就**跳过**重复的 `sampler.on_app_limited()`

并额外输出诊断前缀：

- `BBR_APP_LIMITED_SKIP`

### 验证

- `cargo test` ✅
- `cargo test -p hysteria-core --test runtime -- --ignored` ✅
- 临时拷贝 `vendor/quinn-proto` 单独跑：
  - `env_bool_parses_app_limited_debounce_style_values` ✅
  - `app_limited_trace_every` ✅

### Benchmark

场景仍是：

- `250 ms RTT`
- `30% loss`
- `download`
- `10s x 2`

#### `HY_RS_BBR_APP_LIMITED_SOURCE=hybrid HY_RS_BBR_APP_LIMITED_TARGET_PCT=50`

- `116.765838 Mbps`
- `136.786488 Mbps`

复跑：

- `103.597170 Mbps`
- `122.371836 Mbps`

#### `hybrid + pct50 + debounce`

- `90.064634 Mbps`
- `122.131768 Mbps`

复跑：

- `117.603265 Mbps`
- `140.251831 Mbps`

再次复跑：

- `113.866947 Mbps`
- `92.056276 Mbps`

#### `hybrid + pct50 + debounce + refresh_current`

- `102.328668 Mbps`
- `134.809481 Mbps`

#### `hybrid + pct50 + debounce` 在 `0% loss / 5s x 2`

- `175.731643 Mbps`
- `211.995721 Mbps`

日志：

- `/tmp/hy_bench_20260320_bbr_app_limited_debounce.log`
- `/tmp/hy_bench_20260320_bbr_app_limited_debounce_followup.log`

### Trace 对照

为了判断 debounce 到底有没有缩短 tail，这轮还跑了带 trace 的 `10s x 4`：

- `HY_RS_BBR_APP_LIMITED_SOURCE=hybrid`
- `HY_RS_BBR_APP_LIMITED_TARGET_PCT=50`
- `HY_RS_BBR_APP_LIMITED_DEBOUNCE=1`
- `HY_RS_BBR_APP_LIMITED_TRACE=1`
- `HY_RS_BBR_APP_LIMITED_TRACE_EVERY=128`

日志：

- `/tmp/hy_trace_app_limited_debounce_1774043137/server.log`
- `/tmp/hy_trace_app_limited_debounce_1774043137/bench.log`

和上一轮 baseline trace 相比：

- `maybe_app_limited` 真触发：
  - `898 -> 9`
- `BBR_APP_LIMITED_SKIP`：
  - `0 -> 411`
- `event_app_limited=false` 但 `sample_is_app_limited=true`：
  - `2834 -> 2142`
- `measurement=0`：
  - `2836 -> 2147`

也就是：

- debounce **明显减少了重复 retrigger**
- 也确实减少了大量 “尾巴样本”
- 但**没有把 tail 根治掉**

因为最长 `measurement=0` 连续区间并没有消失，说明：

- 只靠入口去抖
- 还不够解决 ACK 时刻仍被判成 app-limited 的尾巴问题

### 结论

- `debounce` 方向是对的；
- 它能压住 round/stream 边界上的重复 `on_app_limited()`；
- 但**单靠 debounce 本身，还不足以稳定拉高吞吐**；
- 下一刀应该继续处理：
  - **ACK 时刻的 app-limited 尾巴清理**

## 2026-03-20 - ACK 时刻 tail clear：`ack_time_exit_ok`

为了直接处理：

- `event_app_limited=false`
- 但 winning sample 仍沿用了历史 `app_limited` 标记

这轮在 `HY_RS_BBR_MAX_BW_APP_LIMITED` 下新增了一个实验模式：

- `ack_time_exit_ok`

别名：

- `ack_exit_ok`
- `ack_time_clear`
- `tail_clear`

语义是：

- 如果 winning sample 标成 `app_limited`
- 但当前 ACK event 本身已经 **不是** `app_limited`
- 且 sampler 当前也已经退出 `app_limited`
- 那么允许把这次 `max_bw` candidate 当成 **non-app-limited** 处理

### 验证

- `cargo test` ✅
- `cargo test -p hysteria-core --test runtime -- --ignored` ✅
- 临时拷贝 `vendor/quinn-proto` 单独跑：
  - `max_bw_app_limited_strategy` ✅

### Benchmark

场景仍是：

- `250 ms RTT`
- `30% loss`
- `download`
- `10s x 2`

#### baseline：`hybrid + pct50`

- `113.710992 Mbps`
- `85.858364 Mbps`

#### `hybrid + pct50 + ack_time_exit_ok`

- `111.113053 Mbps`
- `118.909505 Mbps`

复跑：

- `116.824149 Mbps`
- `124.108822 Mbps`

#### `hybrid + pct50 + ack_time_exit_ok + debounce`

- `121.043466 Mbps`
- `125.880030 Mbps`

复跑：

- `102.779461 Mbps`
- `121.965888 Mbps`

#### `hybrid + pct50 + ack_time_exit_ok` 在 `0% loss / 5s x 2`

- `170.884987 Mbps`
- `176.222532 Mbps`

日志：

- `/tmp/hy_bench_20260320_bbr_app_limited_tail_gate.log`
- `/tmp/hy_bench_20260320_bbr_app_limited_tail_gate_followup.log`

### `ack_time_exit_ok + debounce` trace 诊断

为了确认它是不是**真的**把 tail 清掉，这轮又补跑了带 trace 的 `10s x 4`：

- `HY_RS_BBR_APP_LIMITED_SOURCE=hybrid`
- `HY_RS_BBR_APP_LIMITED_TARGET_PCT=50`
- `HY_RS_BBR_MAX_BW_APP_LIMITED=ack_time_exit_ok`
- `HY_RS_BBR_APP_LIMITED_DEBOUNCE=1`
- `HY_RS_BBR_APP_LIMITED_TRACE=1`
- `HY_RS_BBR_APP_LIMITED_TRACE_EVERY=128`

日志：

- `/tmp/hy_trace_app_limited_ackexit_debounce_1774044436/server.log`
- `/tmp/hy_trace_app_limited_ackexit_debounce_1774044436/bench.log`

trace bench 摘要：

- `91.763069 Mbps`
- `96.866855 Mbps`
- `93.205767 Mbps`
- `95.174901 Mbps`

> 这轮 trace 很重，吞吐值本身主要用于诊断，不宜直接和无 trace benchmark 横向比较。

### Trace 结果

和前两轮 trace 对比：

- baseline
  - `maybe_app_limited` 真触发：`898`
  - `event_app_limited=false && sample_is_app_limited=true`：`2834`
  - `measurement=0`：`2836`
  - 最长 `measurement=0` 连续区间：`693`
- debounce
  - `maybe_app_limited` 真触发：`9`
  - `BBR_APP_LIMITED_SKIP`：`411`
  - `event_app_limited=false && sample_is_app_limited=true`：`2142`
  - `measurement=0`：`2147`
  - 最长 `measurement=0` 连续区间：`784`
- `ack_time_exit_ok + debounce`
  - `maybe_app_limited` 真触发：`5`
  - `BBR_APP_LIMITED_SKIP`：`76`
  - `event_app_limited=false && sample_is_app_limited=true`：`0`
  - `measurement=0`：`3`
  - 最长 `measurement=0` 连续区间：`1`

剩下那 `3` 个 `measurement=0` 样本里：

- `2` 个发生在连接最早期的 `Startup`
- `1` 个发生在 `ProbeRtt`

也就是说：

- **之前那种 “ACK 已经退出 app-limited，但 sample 还拖着 app-limited 尾巴” 的模式，这轮已经基本消失**

### 结论

这轮 evidence 很强：

- `debounce` 负责减少重复 retrigger；
- `ack_time_exit_ok` 负责在 ACK 时刻把尾巴真正清掉；
- 两者叠加后，trace 里几乎消除了：
  - `event_app_limited=false && sample_is_app_limited=true`
  - 以及大量 `measurement=0` tail

但 benchmark 侧还没到“足够稳定可以直接改默认”的程度，所以目前更合理的结论是：

1. `ack_time_exit_ok` 是目前这条线里**最像有效修正**的一刀；
2. `ack_time_exit_ok + debounce` 也是当前最像值得继续复测的组合；
3. 在真正改默认前，最好再补：
   - 一轮 **`ack_time_exit_ok` 单独 trace**
   - 或更多 **无 trace 重复 benchmark**

### 补充：`ack_time_exit_ok` 单独 trace

随后又补跑了一轮更关键的 isolate trace：

- `HY_RS_BBR_APP_LIMITED_SOURCE=hybrid`
- `HY_RS_BBR_APP_LIMITED_TARGET_PCT=50`
- `HY_RS_BBR_MAX_BW_APP_LIMITED=ack_time_exit_ok`
- `HY_RS_BBR_APP_LIMITED_TRACE=1`
- `HY_RS_BBR_APP_LIMITED_TRACE_EVERY=128`

日志：

- `/tmp/hy_trace_app_limited_ackexit_only_1774044739/server.log`
- `/tmp/hy_trace_app_limited_ackexit_only_1774044739/bench.log`

trace bench 摘要：

- `88.755708 Mbps`
- `110.866400 Mbps`
- `105.256471 Mbps`
- `88.137742 Mbps`

对照 trace 统计：

- `maybe_app_limited` 真触发：`478`
- `event_app_limited=false && sample_is_app_limited=true`：`0`
- `measurement=0`：`8`
- 最长 `measurement=0` 连续区间：`6`

这 `8` 个 `measurement=0` 样本都不是之前那种“退出后尾巴”：

- `6` 个在最早期 `Startup`
- `2` 个在 `ProbeBw`

也就是说：

- **单独开 `ack_time_exit_ok`，已经足够把“ACK 已经退出 app-limited，但 sample 还拖尾”的问题清掉**
- `debounce` 的价值更多体现在：
  - 压低重复 `maybe_app_limited` retrigger
  - 而不是 tail-clear 本身

### 更新后的判断

截至这轮，更合理的优先级变成：

1. **`ack_time_exit_ok` 是当前最强的默认候选**
2. `debounce` 更像一个辅助修补项，而不是必须和 `ack_time_exit_ok` 绑定
3. 下一步最值得做的是：
   - 直接把 **`ack_time_exit_ok` 作为默认候选** 再跑一轮无 trace benchmark

## 2026-03-21 - `ack_time_exit_ok` 改为默认

在补完 trace 之后，这轮直接把：

- `HY_RS_BBR_MAX_BW_APP_LIMITED`

的默认值从：

- `legacy`

改成了：

- `ack_time_exit_ok`

代码位置：

- `vendor/quinn-proto/src/congestion/bbr/mod.rs`
  - `MaxBwAppLimitedStrategy` 默认 variant
  - `MaxBwAppLimitedStrategy::from_env_value(None)`

也同步更新了对应单测：

- `max_bw_app_limited_strategy_parses_and_selects_candidate`

### 验证

- `cargo test` ✅
- `cargo test -p hysteria-core --test runtime -- --ignored` ✅
- `cargo build -p hysteria-app` ✅
- `cargo build --release --manifest-path /tmp/hy_persist_bench/Cargo.toml` ✅

### Benchmark

统一窗口：

```bash
HY_BENCH_STREAM_RWND=268435456
HY_BENCH_CONN_RWND=536870912
HY_SERVER_STREAM_RWND=268435456
HY_SERVER_CONN_RWND=536870912
```

场景：

- `250 ms RTT`
- `30% loss`
- `download`
- `10s x 2`

#### 新默认（`ack_time_exit_ok`）

- `122.634660 Mbps`
- `119.651284 Mbps`

复跑：

- `122.458325 Mbps`
- `112.080898 Mbps`

#### 显式 legacy 对照

- `99.622686 Mbps`
- `122.541850 Mbps`

复跑：

- `109.051031 Mbps`
- `47.998148 Mbps`

#### 新默认在 `0% loss / 5s x 2`

- `156.058822 Mbps`
- `184.732136 Mbps`

日志：

- `/tmp/hy_bench_20260321_ackexit_default.log`

### 结论

这轮数据支持把 `ack_time_exit_ok` 作为默认保留下来：

- 新默认在 `30% loss` 下两轮复跑都落在大约 `112~123 Mbps`
- 显式 legacy 对照则再次出现了明显低样本（`47.998148 Mbps`）
- 结合前一轮 trace，当前更合理的判断是：
  - `ack_time_exit_ok` 已经修正了主要的 post-exit app-limited tail 问题
  - 默认路径因此变得更稳

不过它还不是“最终极限解”：

- 目前默认值仍明显低于历史最好下载样本
- 下一步如果继续攻，优先级仍应回到：
  - `max_bw refresh / startup gating / sampler` 的联动

## 2026-03-21 - `ack_time_exit_ok` 默认下复测 `max_bw refresh + startup gating`

在把：

- `HY_RS_BBR_MAX_BW_APP_LIMITED`

默认切到：

- `ack_time_exit_ok`

之后，这轮继续把之前最像有收益的两组联动旋钮重新复测了一次：

- `HY_RS_BBR_MAX_BW_UPDATE=refresh_current`
- `HY_RS_BBR_STARTUP_FULL_BW_GATE=ignore_app_limited`

目的是确认：

- 这些旋钮在 **新默认** 下还有没有稳定增益；
- 还是说 `ack_time_exit_ok` 已经吃掉了主要收益。

### Benchmark

统一窗口：

```bash
HY_BENCH_STREAM_RWND=268435456
HY_BENCH_CONN_RWND=536870912
HY_SERVER_STREAM_RWND=268435456
HY_SERVER_CONN_RWND=536870912
```

场景：

- `250 ms RTT`
- `30% loss`
- `download`
- `10s x 2`

#### 当前默认（`ack_time_exit_ok`）

- `112.986233 Mbps`
- `107.912876 Mbps`

#### 默认 + `refresh_current`

- `116.329231 Mbps`
- `139.603908 Mbps`

复跑：

- `113.145081 Mbps`
- `83.892160 Mbps`

再次复跑：

- `113.131516 Mbps`
- `113.077977 Mbps`

#### 默认 + `ignore_app_limited`

- `119.539815 Mbps`
- `103.685790 Mbps`

复跑一轮中途 timeout：

- `125.912618 Mbps`
- round 2 timeout

补跑：

- `118.497553 Mbps`
- `128.803265 Mbps`

#### 默认 + `refresh_current + ignore_app_limited`

- `80.469283 Mbps`
- `141.059363 Mbps`

复跑：

- `102.782492 Mbps`
- `150.500409 Mbps`

#### `refresh_current + ignore_app_limited` 在 `0% loss / 5s x 2`

- `155.670353 Mbps`
- `185.348994 Mbps`

日志：

- `/tmp/hy_bench_20260321_ackexit_linked_knobs.log`
- `/tmp/hy_bench_20260321_ackexit_linked_knobs_followup.log`

### 结论

这轮更像说明：

1. **`ack_time_exit_ok` 已经吃掉了这条线里的主要稳定收益**
2. `refresh_current` 仍有上探空间，但波动依然明显
3. `ignore_app_limited` 也有正收益信号，但这轮还出现了 round 2 timeout
4. `refresh_current + ignore_app_limited` 高点更高，但离散度更大，不适合直接继续上默认

所以截至这轮：

- 默认继续保持：
  - `HY_RS_BBR_MAX_BW_APP_LIMITED=ack_time_exit_ok`
- 但**不继续叠默认**：
  - `HY_RS_BBR_MAX_BW_UPDATE=refresh_current`
  - `HY_RS_BBR_STARTUP_FULL_BW_GATE=ignore_app_limited`

更合理的下一步还是：

- 回到 **sampler / max_bw refresh / startup round 联动本体**
- 而不是继续把这两个旧旋钮直接叠到新默认上

## 2026-03-21 - `ack_time_exit_ok` 默认下复测 STARTUP 参数

既然：

- `ack_time_exit_ok`

已经被证实值得保留为默认，这轮继续回到 STARTUP 本体，专门复测两个已有旋钮：

- `HY_RS_BBR_STARTUP_ROUNDS`
- `HY_RS_BBR_STARTUP_GROWTH`

目标是看：

- 当前默认是不是还存在 **过早退出 STARTUP**
- 以及在新默认下，放宽 STARTUP 判定是否还能稳定提高 `30% loss` 下载

### Benchmark

统一窗口：

```bash
HY_BENCH_STREAM_RWND=268435456
HY_BENCH_CONN_RWND=536870912
HY_SERVER_STREAM_RWND=268435456
HY_SERVER_CONN_RWND=536870912
```

场景：

- `250 ms RTT`
- `30% loss`
- `download`
- `10s x 2`

#### baseline（当前默认）

- `118.240598 Mbps`
- `116.691695 Mbps`

#### `HY_RS_BBR_STARTUP_ROUNDS=8`

- `117.569127 Mbps`
- `128.175562 Mbps`

复跑：

- `124.046515 Mbps`
- `112.336534 Mbps`

再跑一轮：

- `109.713427 Mbps`
- `124.886335 Mbps`

#### `HY_RS_BBR_STARTUP_GROWTH=1.15`

- `126.417125 Mbps`
- `111.193386 Mbps`

一次复跑发生 timeout

补跑：

- `118.710339 Mbps`
- `144.152230 Mbps`

再补一轮：

- `119.459667 Mbps`
- `110.170843 Mbps`

#### `HY_RS_BBR_STARTUP_ROUNDS=8 HY_RS_BBR_STARTUP_GROWTH=1.15`

- `107.334013 Mbps`
- `140.685543 Mbps`

复跑：

- `106.480324 Mbps`
- `94.762309 Mbps`

### `0% loss / 5s x 2`

#### `HY_RS_BBR_STARTUP_ROUNDS=8`

- `179.197517 Mbps`
- `203.662277 Mbps`

#### `HY_RS_BBR_STARTUP_GROWTH=1.15`

- `183.939425 Mbps`
- `208.758120 Mbps`

日志：

- `/tmp/hy_bench_20260321_ackexit_startup_knobs.log`
- `/tmp/hy_bench_20260321_ackexit_startup_knobs_followup.log`
- `/tmp/hy_bench_20260321_ackexit_startup_growth115_confirm.log`

### 结论

这轮结果说明：

1. `STARTUP` 这条线在新默认下**仍然有收益空间**
2. `HY_RS_BBR_STARTUP_ROUNDS=8` 有**温和正收益**，而且相对更稳
3. `HY_RS_BBR_STARTUP_GROWTH=1.15` 的上探更明显，但也出现过一次 timeout，**稳定性还不够**
4. 两者直接叠加后反而更散，不值得继续一起推

如果按目前证据排序：

1. **最值得继续观察的 STARTUP 旋钮：`HY_RS_BBR_STARTUP_GROWTH=1.15`**
2. **更稳但收益较温和的候选：`HY_RS_BBR_STARTUP_ROUNDS=8`**

但截至这轮，证据还**不足以继续改默认**。

更合理的下一步是二选一：

- 要么给 STARTUP exit / full_bw check 加轻量 trace，确认 timeout 和低样本是不是来自过早退出；
- 要么继续围绕 `HY_RS_BBR_STARTUP_GROWTH=1.15` 做更多无 trace 复测。

## 2026-03-21 - STARTUP / full_bw trace 诊断

这轮沿着上一轮的判断，给 STARTUP 路径加了一个**默认关闭**的轻量 trace：

- `HY_RS_BBR_STARTUP_TRACE=1`

日志前缀：

- `BBR_STARTUP_CHECK`
- `BBR_STARTUP_MODE`

trace 点覆盖：

- `check_if_full_bw_reached()`
- `maybe_exit_startup_or_drain()`

也补了一个很小的解析测试：

- `env_bool_parses_startup_trace_style_values`

### 验证

- `cargo test` ✅
- `cargo test -p hysteria-core --test runtime -- --ignored` ✅
- `cargo build -p hysteria-app` ✅
- `cargo build --release --manifest-path /tmp/hy_persist_bench/Cargo.toml` ✅

### Trace：当前默认 vs `HY_RS_BBR_STARTUP_GROWTH=1.15`

场景：

- `250 ms RTT`
- `30% loss`
- `download`
- `10s x 4`

#### 当前默认

日志：

- `/tmp/hy_trace_startup_default_1774066277/server.log`
- `/tmp/hy_trace_startup_default_1774066277/bench.log`

trace bench 摘要：

- `109.916816 Mbps`
- `129.522371 Mbps`
- `126.847131 Mbps`
- `17.796992 Mbps`

trace 统计：

- `BBR_STARTUP_CHECK`：`118`
- `stage=skip`：`114`
- `stage=target_hit`：`3`
- `stage=no_gain`：`1`
- `startup_to_drain`：`0`
- `drain_to_probe_bw`：`0`

最终状态：

- round `118`
- mode 仍然是 `Startup`
- `is_at_full_bandwidth=false`

#### `HY_RS_BBR_STARTUP_GROWTH=1.15`

日志：

- `/tmp/hy_trace_startup_growth115_1774066323/server.log`
- `/tmp/hy_trace_startup_growth115_1774066323/bench.log`

trace bench 摘要：

- `106.689764 Mbps`
- `131.162226 Mbps`
- `120.782106 Mbps`
- `22.528620 Mbps`

trace 统计：

- `BBR_STARTUP_CHECK`：`121`
- `stage=skip`：`117`
- `stage=target_hit`：`3`
- `stage=no_gain`：`1`
- `startup_to_drain`：`0`
- `drain_to_probe_bw`：`0`

最终状态：

- round `120`
- mode 仍然是 `Startup`
- `is_at_full_bandwidth=false`

### 关键结论

这轮 trace 把问题直接定位出来了：

- **当前默认并不是“过早退出 STARTUP”**
- 而是 **大多数轮次根本没有执行有效的 full_bw check**

因为从 round `4` 左右开始：

- `last_sample_is_app_limited=true`

于是默认 gate 会不断跳过：

- `check_if_full_bw_reached()`

结果就是：

- `HY_RS_BBR_STARTUP_GROWTH=1.15` 这类参数在默认 gate 下**基本发挥不出来**
- STARTUP 在很多样本里会长期停留在：
  - `mode=Startup`
  - `is_at_full_bandwidth=false`

### Trace：`HY_RS_BBR_STARTUP_FULL_BW_GATE=ignore_app_limited`

为了确认是不是这个 gate 本身在卡住 full_bw check，又补跑了一轮：

- `HY_RS_BBR_STARTUP_TRACE=1`
- `HY_RS_BBR_STARTUP_FULL_BW_GATE=ignore_app_limited`

日志：

- `/tmp/hy_trace_startup_ignore_gate_1774066499/server.log`
- `/tmp/hy_trace_startup_ignore_gate_1774066499/bench.log`

trace bench 摘要：

- `100.445265 Mbps`
- `120.910519 Mbps`
- `106.757392 Mbps`
- `139.016962 Mbps`

trace 统计：

- `BBR_STARTUP_CHECK`：`19`
- `stage=skip`：`0`
- `stage=target_hit`：`9`
- `stage=no_gain`：`10`
- `startup_to_drain`：`1`
- `drain_to_probe_bw`：`1`

关键事件：

- 在 round `19`
- 因为 `round_wo_bw_gain=6`
- 触发了 `startup_to_drain`
- `exit_due_to_loss=false`

也就是说：

- **一旦不再因为 app-limited 直接跳过 check，STARTUP 就能正常完成 full_bw 判定并退出**
- 而且这轮退出原因是：
  - **连续无足够增长**
  - 不是 loss gate

### 进一步对照：`ignore_app_limited + STARTUP_GROWTH=1.15`

既然 trace 说明 `STARTUP_GROWTH` 只有在 gate 放开后才真正有机会生效，这轮又补了无 trace benchmark：

- `HY_RS_BBR_STARTUP_FULL_BW_GATE=ignore_app_limited`
- `HY_RS_BBR_STARTUP_GROWTH=1.15`

日志：

- `/tmp/hy_bench_20260321_startup_gate_growth_combo.log`

#### `ignore_app_limited` baseline

- `124.706981 Mbps`
- `113.520311 Mbps`

#### `ignore_app_limited + STARTUP_GROWTH=1.15`

- `98.658935 Mbps`
- `111.434764 Mbps`

复跑：

- `107.413179 Mbps`
- `136.419111 Mbps`

#### `ignore_app_limited + STARTUP_GROWTH=1.15` 在 `0% loss / 5s x 2`

- `169.556609 Mbps`
- `215.316544 Mbps`

### 更新后的判断

截至这轮，结论比上一轮更清楚：

1. `STARTUP_GROWTH=1.15` 并不是当前主因；
2. 当前真正卡住 STARTUP 的，是：
   - **`startup_full_bw_gate` 在 app-limited 样本下几乎一直 skip**
3. 所以之前看到的 `STARTUP_GROWTH=1.15` 小幅收益，很多更像：
   - **噪声叠加**
   - 而不是稳定修正；
4. 如果要继续攻 STARTUP，本质上应该继续查：
   - **`last_sample_is_app_limited` 为什么在 STARTUP 里长期保持 true**
   - 以及是否需要更细的 **STARTUP 专用 app-limited gate**

因此这轮之后：

- **不建议把 `HY_RS_BBR_STARTUP_GROWTH=1.15` 改成默认**
- 也**不建议仅凭旧 benchmark 继续押注“过早退出 STARTUP”**

更合理的下一步是：

- 直接沿着 **STARTUP 内部的 app-limited 判定 / gate 语义** 继续查

## 2026-03-21 - STARTUP gate 语义继续细化

基于上一轮 trace：

- 默认 gate 会因为 `last_sample_is_app_limited=true` 长时间跳过 full_bw check

这轮继续沿着 STARTUP gate 语义做了两步：

1. 扩展 `HY_RS_BBR_STARTUP_TRACE=1` 的输出
   - 在 `BBR_STARTUP_CHECK` 里补了：
     - `sample_is_app_limited`
     - `has_non_app_limited_sample`
     - `event_app_limited`
     - `sampler_is_app_limited`
2. 在 `HY_RS_BBR_STARTUP_FULL_BW_GATE` 下新增两个实验模式：
   - `ack_time_exit_ok`
   - `event_only`

代码：

- `vendor/quinn-proto/src/congestion/bbr/mod.rs`

测试也同步补了：

- `startup_full_bw_gate_strategy_parses_and_applies`

### 验证

- `cargo test` ✅
- `cargo test -p hysteria-core --test runtime -- --ignored` ✅
- `cargo build -p hysteria-app` ✅
- `cargo build --release --manifest-path /tmp/hy_persist_bench/Cargo.toml` ✅

### Trace：`HY_RS_BBR_STARTUP_FULL_BW_GATE=ack_time_exit_ok`

日志：

- `/tmp/hy_trace_startup_gate_ackexit_1774069271/server.log`
- `/tmp/hy_trace_startup_gate_ackexit_1774069271/bench.log`

trace bench 摘要：

- `116.195089 Mbps`
- `105.646856 Mbps`
- `137.572507 Mbps`
- `0.137595 Mbps`

trace 统计：

- `BBR_STARTUP_CHECK`：`108`
- `stage=skip`：`105`
- `stage=target_hit`：`3`
- `startup_to_drain`：`0`
- `drain_to_probe_bw`：`0`

进一步看 skip 上下文：

- `skip_total=105`
- 其中 `event_app_limited=false`：`97`
- 但 `sampler_is_app_limited=true`：几乎一直成立
- 同时 `has_non_app_limited_sample=false`

这说明：

- `ack_time_exit_ok` 这套 gate 语义对 STARTUP 来说**还是太保守**
- 因为 sampler 的 app-limited phase 太粘，导致 full_bw check 依然几乎一直被 skip

### Trace：`HY_RS_BBR_STARTUP_FULL_BW_GATE=event_only`

日志：

- `/tmp/hy_trace_startup_gate_eventonly_1774069562/server.log`
- `/tmp/hy_trace_startup_gate_eventonly_1774069562/bench.log`

trace bench 摘要：

- `122.275122 Mbps`
- `130.452192 Mbps`
- `118.728492 Mbps`
- `115.242282 Mbps`

trace 统计：

- `BBR_STARTUP_CHECK`：`20`
- `stage=skip`：`4`
- `stage=target_hit`：`5`
- `stage=no_gain`：`11`
- `startup_to_drain`：`1`
- `drain_to_probe_bw`：`1`

也就是说：

- `event_only` 确实大幅放开了 full_bw check
- 并且能让 STARTUP **正常退出到 Drain / ProbeBw**

### 无 trace benchmark

日志：

- `/tmp/hy_bench_20260321_startup_gate_eventonly.log`
- `/tmp/hy_bench_20260321_startup_gate_eventonly_rounds8.log`
- `/tmp/hy_bench_20260321_ignore_gate_rounds8_followup.log`

场景仍是：

- `250 ms RTT`
- `30% loss`
- `download`
- `10s x 2`

#### `event_only`

- 一次直接 timeout，round 1 只有：`27.960582 Mbps`
- 复跑：
  - `118.030717 Mbps`
  - `85.679442 Mbps`

#### `event_only + STARTUP_GROWTH=1.15`

- `32.279261 Mbps`
- `34.164836 Mbps`

复跑：

- `109.436874 Mbps`
- `95.392646 Mbps`

#### `event_only + STARTUP_ROUNDS=8`

- 一次直接 timeout
- 复跑：
  - `114.439777 Mbps`
  - `42.563657 Mbps`

### 额外对照：`ignore_app_limited + STARTUP_ROUNDS=8`

这组一开始看起来不错：

- `124.026928 Mbps`
- `136.791780 Mbps`

复跑：

- `128.907415 Mbps`
- `127.051374 Mbps`

但补跑又出现明显塌陷：

- `35.774664 Mbps`
- `26.083173 Mbps`

和 `refresh_current` 叠加：

- `108.742757 Mbps`
- `124.072443 Mbps`

`0% loss / 5s x 2`：

- `176.751149 Mbps`
- `178.000717 Mbps`

### 结论

这轮把 STARTUP gate 线索又收窄了一步：

1. **`ack_time_exit_ok` 语义对 STARTUP 还不够强**
   - 因为 sampler 的 app-limited phase 太粘
2. **`event_only` 语义能让 STARTUP 真的完成 full_bw 判定并退出**
3. 但 `event_only` 以及 `ignore_app_limited + STARTUP_ROUNDS=8` 都还**不够稳**
   - 会出现 timeout
   - 或明显低样本塌陷

所以截至这轮：

- **没有新的 STARTUP gate 值得改默认**
- 但已经可以更明确地说：
  - 问题不在 “growth 参数本身”
  - 而在 **STARTUP 里 app-limited gate 的语义过于粗糙**

如果继续，下一步更合理的是：

- 继续查 **sampler app_limited phase 为什么在 STARTUP 里这么粘**
- 而不是继续叠更多 STARTUP growth/round 参数

## 2026-03-21 - sampler app-limited exit 去粘连

这轮直接改 `BandwidthSampler` 本体，新增了一个**默认关闭**的实验开关：

- `HY_RS_BBR_SAMPLER_APP_LIMITED_EXIT=ack_time_exit_ok`

位置：

- `vendor/quinn-proto/src/congestion/bbr/bw_estimation.rs`

语义：

- legacy：
  - 只有当 ACK 到了 **严格大于** `end_of_app_limited_phase` 的包，才退出 sampler 的 app-limited phase
- `ack_time_exit_ok`：
  - 如果当前 ACK event 已经 `event_app_limited=false`
  - 且 ACK 到了 `end_of_app_limited_phase` 本身
  - 就允许提前清掉 sampler 的 app-limited phase

也就是把之前那种：

- `ACK event 已经不再 app-limited`
- 但 sampler 还要继续拖一段 tail

的情况，尽量在边界 ACK 当下就收掉。

### 验证

- `cargo test` ✅
- `cargo test -p hysteria-core --test runtime -- --ignored` ✅
- `cargo build -p hysteria-app` ✅
- `cargo build --release --manifest-path /tmp/hy_persist_bench/Cargo.toml` ✅

### Trace 对照：`hybrid + pct50 + STARTUP_FULL_BW_GATE=ack_time_exit_ok`

场景：

- `250 ms RTT`
- `30% loss`
- `download`
- `10s x 2`

#### control：**不**开 sampler exit

日志：

- `/tmp/hy_trace_sampler_exit_control_125_1774077510/server.log`
- `/tmp/hy_trace_sampler_exit_control_125_1774077510/bench.log`

trace bench 摘要：

- `39.659011 Mbps`
- `41.053272 Mbps`

trace 统计：

- `BBR_STARTUP_CHECK`：`86`
- `stage=skip`：`78`
- `stage=target_hit`：`6`
- `stage=no_gain`：`2`
- `sampler_is_app_limited=true` 的 check：`80`
- `startup_to_drain`：`0`
- `drain_to_probe_bw`：`0`

#### experiment：开 `HY_RS_BBR_SAMPLER_APP_LIMITED_EXIT=ack_time_exit_ok`

日志：

- `/tmp/hy_trace_sampler_exit_ackexit_125_1774077440/server.log`
- `/tmp/hy_trace_sampler_exit_ackexit_125_1774077440/bench.log`

trace bench 摘要：

- `98.392691 Mbps`
- `122.416425 Mbps`

trace 统计：

- `BBR_STARTUP_CHECK`：`23`
- `stage=skip`：`5`
- `stage=target_hit`：`8`
- `stage=no_gain`：`10`
- `sampler_is_app_limited=true` 的 check：`7`
- `startup_to_drain`：`1`
- `drain_to_probe_bw`：`1`

这说明：

- sampler exit 这一刀**确实打中了根因**
- `ack_time_exit_ok` STARTUP gate 之前之所以不够用，主要就是因为 sampler phase 太粘
- 一旦把 sampler tail 清短，STARTUP full_bw check 就会从“几乎一直 skip”变成“可以正常工作并退出”

### 无 trace benchmark

日志：

- `/tmp/hy_bench_20260321_sampler_exit_125.log`
- `/tmp/hy_bench_20260321_sampler_exit_compare.log`
- `/tmp/hy_bench_20260321_sampler_exit_compare_repeat.log`

统一场景：

- `250 ms RTT`
- `30% loss`
- `download`
- `10s x 2`

#### baseline（当前默认）

- `28.594083 Mbps`
- `38.067564 Mbps`

#### 只开 `HY_RS_BBR_SAMPLER_APP_LIMITED_EXIT=ack_time_exit_ok`

- 一次失败：`connection lost / timed out`

说明：

- 单独清 sampler tail，**还不够**
- 因为当前默认 STARTUP gate 仍然主要看 `last_sample_is_app_limited`

#### `HY_RS_BBR_APP_LIMITED_SOURCE=hybrid HY_RS_BBR_APP_LIMITED_TARGET_PCT=50`

- `94.592977 Mbps`
- `99.704299 Mbps`

#### `hybrid + pct50 + sampler_exit`

- `106.020936 Mbps`
- `64.924582 Mbps`

这组有改善信号，但还不稳。

#### `hybrid + pct50 + STARTUP_FULL_BW_GATE=ack_time_exit_ok`

第一轮：

- `108.277012 Mbps`
- `111.031555 Mbps`

复跑：

- `116.201766 Mbps`
- `120.746342 Mbps`

#### `hybrid + pct50 + sampler_exit + STARTUP_FULL_BW_GATE=ack_time_exit_ok`

一次较差样本：

- `37.683249 Mbps`
- `28.872330 Mbps`

后续两轮：

- `111.777232 Mbps`
- `129.585646 Mbps`

复跑：

- `122.272045 Mbps`
- `123.569997 Mbps`

`0% loss / 5s x 2`：

- `200.523618 Mbps`
- `199.271657 Mbps`

### 结论

这一轮的关键结论是：

1. **sampler app-limited exit 的 sticky tail 确实是主因之一**
2. `HY_RS_BBR_SAMPLER_APP_LIMITED_EXIT=ack_time_exit_ok`
   - 能显著减少 STARTUP check 被 `sampler_is_app_limited` 卡住的次数
   - 并让 `HY_RS_BBR_STARTUP_FULL_BW_GATE=ack_time_exit_ok` 终于具备可用性
3. 但就无 trace benchmark 来看：
   - **sampler_exit 单独开还不够**
   - 目前最像有效组合的是：
     - `HY_RS_BBR_APP_LIMITED_SOURCE=hybrid`
     - `HY_RS_BBR_APP_LIMITED_TARGET_PCT=50`
     - `HY_RS_BBR_STARTUP_FULL_BW_GATE=ack_time_exit_ok`
     - 可再叠 `HY_RS_BBR_SAMPLER_APP_LIMITED_EXIT=ack_time_exit_ok`
4. 不过这个组合仍出现过一次明显低样本，所以**还不适合直接改默认**

当前更合理的判断是：

- `sampler_exit` 已经把问题从“语义不对”推进到了“组合稳定性还需验证”
- 下一步该继续看：
  - `hybrid+pct50+startup_gate+sampler_exit` 为什么仍会偶发塌陷
  - 以及这套组合是否能在更多轮次下稳定优于 control

## 2026-03-21 - 偶发塌陷进一步定位到 ProbeRtt

这轮继续追 `hybrid+pct50+startup_gate+sampler_exit` 的偶发低样本，先加了一个**默认关闭**的轻量状态 trace：

- `HY_RS_BBR_STATE_TRACE=1`

位置：

- `vendor/quinn-proto/src/congestion/bbr/mod.rs`

trace 会打：

- mode transition
- recovery transition
- round-start / loss ACK event 的状态快照

格式前缀统一为：

- `BBR_STATE`

### 关键定位结果

我用 `HY_RS_BBR_STATE_TRACE=1` 重跑组合，终于抓到一个明显低样本：

日志：

- `/tmp/hy_state_trace_combo_1774079740_8/server.log`
- `/tmp/hy_state_trace_combo_1774079740_8/bench.log`

结果：

- round 1：`96.009717 Mbps`
- round 2：`28.839823 Mbps`

这次不是 recovery 卡住，也不是 STARTUP 退不出去。  
真正的触发点是：

- `BBR_STATE ... detail=enter_probe_rtt`
- 出现在 `round=51`
- 当时：
  - `mode=ProbeBw -> ProbeRtt`
  - `bytes_in_flight=16609117`
  - `sampler_is_app_limited=false`

而且之后**没有出现**：

- `exit_probe_rtt_to_probe_bw`

也就是说：

- 这次低样本的主因，是 **ProbeRtt 在高 inflight 下被触发**
- 进入后又因为 inflight 不容易降到 probe-rtt cwnd，导致在 benchmark 尾段长时间卡在 `ProbeRtt`

对照一个正常样本：

- `/tmp/hy_state_trace_combo_1774079682_6/server.log`

里面能看到：

- `enter_probe_rtt`
- 紧接着下一轮就有：
  - `exit_probe_rtt_to_probe_bw`

说明：

- 问题不是 “进入 ProbeRtt 一定错”
- 而是 **在 busy / high-inflight 状态进入 ProbeRtt** 容易把吞吐拖垮

### 新实验开关

基于上面的定位，我加了一个新的**默认关闭**实验旋钮：

- `HY_RS_BBR_PROBE_RTT_ENTRY=idle_or_drain`

位置：

- `vendor/quinn-proto/src/congestion/bbr/mod.rs`

语义：

- legacy：`min_rtt_expired` 就直接进入 `ProbeRtt`
- `idle_or_drain`：
  - 只有满足以下任一条件才允许进入：
    - `event_app_limited=true`
    - `sampler_is_app_limited=true`
    - `bytes_in_flight <= probe_rtt_cwnd`

也就是把 `ProbeRtt` 的进入时机限制到：

- 已经偏 idle
- 或已经基本 drain 下来

而不是在高 inflight 的 busy phase 里硬切进去。

### 验证

- `cargo test` ✅
- `cargo test -p hysteria-core --test runtime -- --ignored` ✅
- `cargo build -p hysteria-app` ✅
- `cargo build --release --manifest-path /tmp/hy_persist_bench/Cargo.toml` ✅

### 带状态 trace 的一次对照

日志：

- `/tmp/hy_state_trace_probe_rtt_idle_1774080469/server.log`
- `/tmp/hy_state_trace_probe_rtt_idle_1774080469/bench.log`

组合：

- `HY_RS_BBR_APP_LIMITED_SOURCE=hybrid`
- `HY_RS_BBR_APP_LIMITED_TARGET_PCT=50`
- `HY_RS_BBR_STARTUP_FULL_BW_GATE=ack_time_exit_ok`
- `HY_RS_BBR_SAMPLER_APP_LIMITED_EXIT=ack_time_exit_ok`
- `HY_RS_BBR_PROBE_RTT_ENTRY=idle_or_drain`
- `HY_RS_BBR_STATE_TRACE=1`

结果：

- `114.384901 Mbps`
- `124.822783 Mbps`

transition：

- 只看到了：
  - `startup_to_drain`
  - `drain_to_probe_bw`
- **没有看到 `enter_probe_rtt`**

这说明：

- 新 gate 至少能阻止那种 “busy phase 高 inflight 直接切 ProbeRtt” 的情况

### 无 trace benchmark

日志：

- `/tmp/hy_bench_20260321_probe_rtt_entry_compare.log`

场景：

- `250 ms RTT`
- `30% loss`
- `download`
- `10s x 2`

#### 当前组合

- `121.823728 / 135.386098 Mbps`
- `113.541521 / 120.848796 Mbps`
- `120.351913 / 137.889505 Mbps`
- `118.344508 / 130.773425 Mbps`
- `100.867337 / 143.353140 Mbps`

#### `+ HY_RS_BBR_PROBE_RTT_ENTRY=idle_or_drain`

- `112.265927 / 133.593576 Mbps`（一次 connect retry）
- `129.678816 / 138.268057 Mbps`
- `110.096712 / 127.032605 Mbps`
- `103.364699 / 141.950135 Mbps`（一次 connect retry）
- `126.686109 / 137.187011 Mbps`

`0% loss / 5s x 2`：

- 当前组合：`194.430927 / 205.398902 Mbps`
- `idle_or_drain`：`192.694634 / 203.748919 Mbps`

### 扩大样本批量复测

日志根目录：

- `/tmp/hy_probe_rtt_batch_20260321/`

汇总：

- `/tmp/hy_probe_rtt_batch_20260321/results.json`

场景仍是：

- `250 ms RTT`
- `30% loss`
- `download`
- `10s x 2`

每组各跑了 **8 个 iteration**。

#### 当前组合

16 个 round 的汇总：

- `min`: `9.147539 Mbps`
- `median`: `115.594350 Mbps`
- `mean`: `106.862982 Mbps`
- `< 50 Mbps`：`1/16`
- `< 80 Mbps`：`1/16`
- `connect_fail` retry：`1/8 iter`

单次最差样本出现在：

- iter 6
- `112.565222 / 9.147539 Mbps`

#### `+ HY_RS_BBR_PROBE_RTT_ENTRY=idle_or_drain`

16 个 round 的汇总：

- `min`: `106.262919 Mbps`
- `median`: `122.648580 Mbps`
- `mean`: `121.955945 Mbps`
- `< 50 Mbps`：`0/16`
- `< 80 Mbps`：`0/16`
- `connect_fail` retry：`2/8 iter`

也就是说这批扩大样本下：

- `idle_or_drain` **完全消掉了这轮里观测到的低样本尾部**
- 同时把中位数和均值都往上推了一截

#### `0% loss / 5s x 2` 扩大样本

每组各跑了 **3 个 iteration**：

当前组合：

- `min`: `180.159941 Mbps`
- `median`: `199.734643 Mbps`
- `mean`: `198.978762 Mbps`

`idle_or_drain`：

- `min`: `174.134359 Mbps`
- `median`: `184.723067 Mbps`
- `mean`: `188.824485 Mbps`

这说明：

- `idle_or_drain` 在高 loss 下载方向的稳定性收益更明显
- 但 `0% loss` 下也确实有一点轻微回落

### 结论

这轮的结论是：

1. **偶发塌陷的又一个直接原因已经抓到了：ProbeRtt 在高 inflight 下进入**
2. 这和前面查到的 sampler / STARTUP 问题是**不同层面的第二个坑**
3. `HY_RS_BBR_PROBE_RTT_ENTRY=idle_or_drain`
   - 方向上合理
   - 能阻止已观测到的坏型 `enter_probe_rtt`
   - 目前没看到 0% loss 明显副作用
4. 扩大样本后，`idle_or_drain` 的证据已经更强：
   - 在这批 `30% loss` 复测里明显更稳
   - 并消掉了当前组合仍会出现的 `9 Mbps` 级低样本
5. 但 `0% loss` 也出现了轻微回落
   - 所以它现在更像是 **高 loss 场景的强候选**
   - 还不适合在没有更多覆盖面的情况下直接改成通用默认

当前更合理的判断是：

- `probe_rtt_entry=idle_or_drain` 已经是一个**值得继续保留的候选修正**
- 甚至已经接近“高 loss 默认候选”的强度
- 下一步应该继续：
  - 看是否能在更多覆盖面下维持这个稳定收益
  - 尤其要再查清 `0% loss` 的轻微回落是不是稳定存在

## 2026-03-21 - 扩大覆盖面复测 `idle_or_drain`

这轮按你的要求，专门扩大了：

- `0% loss`
- `低 loss`（`5%` / `10%`）
- `不同 RTT`

的覆盖面，重点看：

- `HY_RS_BBR_PROBE_RTT_ENTRY=idle_or_drain`

到底是不是一个稳定的通用候选。

### 统一对照组合

baseline 组合：

- `HY_RS_BBR_APP_LIMITED_SOURCE=hybrid`
- `HY_RS_BBR_APP_LIMITED_TARGET_PCT=50`
- `HY_RS_BBR_STARTUP_FULL_BW_GATE=ack_time_exit_ok`
- `HY_RS_BBR_SAMPLER_APP_LIMITED_EXIT=ack_time_exit_ok`

candidate 组合：

- baseline
- `+ HY_RS_BBR_PROBE_RTT_ENTRY=idle_or_drain`

### 矩阵复测（首轮）

日志根目录：

- `/tmp/hy_probe_rtt_matrix_20260321/`

汇总：

- `/tmp/hy_probe_rtt_matrix_20260321/results.json`

场景：

- 单向 delay：`25 / 125 / 250 ms`
- 总 RTT：`50 / 250 / 500 ms`
- loss：`0 / 5 / 10 %`
- download
- `10s x 2`

#### RTT 50 ms

`0% loss`

- current：`391.367150 / 445.974719`
- idle_or_drain：`420.132435 / 465.421217`

`5% loss`

- current：`377.526660 / 416.680063`
- idle_or_drain：`376.754412 / 415.549403`

`10% loss`

- current：`330.770763 / 317.053409`
- idle_or_drain：`320.771379 / 334.259755`

结论：

- **RTT 50 ms 下几乎没有副作用**
- `0%` 甚至还有一点正收益

#### RTT 250 ms

`0% loss`

- current：`188.134432 / 193.333402`
- idle_or_drain：`181.199091 / 194.316879`

`5% loss`

- current：`139.163776 / 135.454668`
- idle_or_drain：`137.639010 / 144.609842`

`10% loss`

- current：`157.823514 / 153.173754`
- idle_or_drain：`124.874463 / 145.159934`

结论：

- RTT 250 ms 下：
  - `0%` 基本接近
  - `5%` 看起来略有正收益
  - `10%` 首轮看起来偏负

所以这里需要补测确认。

#### RTT 500 ms

`0% loss`

- current：`121.382525 / 150.538192`
- idle_or_drain：`117.788917 / 148.612523`

`5% loss`

- current：`71.696444 / 106.179949`
- idle_or_drain：`68.313235 / 87.750294`

`10% loss`

- current：`51.935217 / 43.946396`
- idle_or_drain：首轮直接 timeout

结论：

- **RTT 500 ms 下开始出现明显副作用信号**
- 特别是 `10% loss` 首轮就出现 timeout

### 定向补测（重点复核疑似副作用格子）

日志根目录：

- `/tmp/hy_probe_rtt_followup_20260321/`

汇总：

- `/tmp/hy_probe_rtt_followup_20260321/results.json`

补测了这些格子，各 **3 个 iteration**：

- `RTT 250 ms / 0%`
- `RTT 250 ms / 10%`
- `RTT 500 ms / 0%`
- `RTT 500 ms / 5%`
- `RTT 500 ms / 10%`

#### RTT 250 ms / 0%

6 个 round 汇总：

- current：
  - mean `188.693964`
  - median `187.349745`
  - min `176.484830`
- idle_or_drain：
  - mean `188.548695`
  - median `189.273033`
  - min `173.029722`

结论：

- **基本持平**

#### RTT 250 ms / 10%

6 个 round 汇总：

- current：
  - mean `148.117622`
  - median `141.522112`
  - min `126.813951`
- idle_or_drain：
  - mean `148.081043`
  - median `148.936207`
  - min `127.832832`

结论：

- 补测后也变成了**几乎持平**
- 首轮那次明显负差更像单次波动

#### RTT 500 ms / 0%

6 个 round 汇总：

- current：
  - mean `129.365139`
  - median `123.834376`
- idle_or_drain：
  - mean `132.082032`
  - median `135.268260`

结论：

- **0% loss 下没有稳定副作用**
- 甚至略偏正

#### RTT 500 ms / 5%

6 个 round 汇总：

- current：
  - mean `92.054406`
  - median `92.609815`
  - min `71.874442`
- idle_or_drain：
  - mean `88.066704`
  - median `91.757815`
  - min `58.797330`

结论：

- **有轻微负收益**
- 但不是灾难性

#### RTT 500 ms / 10%

6 个 round 汇总：

- current：
  - mean `82.821942`
  - median `76.337897`
  - min `57.997778`
- idle_or_drain：
  - mean `65.341142`
  - median `61.542996`
  - min `51.431212`

补充：

- 首轮矩阵里这个格子还有过一次 timeout
- 补测 3 次虽然没再 timeout，但平均值仍明显更差

结论：

- **RTT 500 ms + 10% loss 下，`idle_or_drain` 的负副作用是比较可信的**

### 合并判断

把首轮矩阵和补测一起看，现在更像是：

1. **RTT 50 ms**
   - `idle_or_drain` 基本无副作用

2. **RTT 250 ms**
   - `0%` 和 `10%` 最终看基本持平
   - 所以这里没有明显通用副作用证据

3. **RTT 500 ms**
   - `0%` 基本持平
   - `5%` 有轻微负收益
   - `10%` 有较明确负收益

也就是说：

- `idle_or_drain` **不是一个“全局更优”的通用默认**
- 它更像是：
  - 在一部分 **中等 RTT / 高 loss / 容易误进 ProbeRtt** 的场景里有稳定性收益
  - 但在 **更高 RTT（500 ms）+ 中低 loss（5~10%）** 下，可能因为过度推迟 ProbeRtt 而带来代价

### 额外状态 trace（RTT 500 ms / 10%）

为了确认不是别的新问题，我又补了两条状态 trace：

- current：
  - `/tmp/hy_state_trace_rtt500_loss10_current_1774091743/`
- idle_or_drain：
  - `/tmp/hy_state_trace_rtt500_loss10_idle_or_drain_1774091771/`

这一对 trace 说明：

- current：
  - 会在 `round=17` 进入 `ProbeRtt`
  - 并在 `round=19` 退出到 `ProbeBw`
- idle_or_drain：
  - 这次没有进入 `ProbeRtt`
  - 吞吐是 `66.935744 / 107.187468 Mbps`

所以这里更像是：

- `idle_or_drain` 的问题**不是**又引入了新的卡死模式
- 而是它在极高 RTT 场景里，可能确实把 `ProbeRtt` 推迟过头，带来模型更新代价

### 当前结论

到这一步，关于 `idle_or_drain` 可以更明确地说：

1. **它不是安全的全局默认候选**
2. **它在 RTT 500 ms + 5~10% loss 下存在可信负副作用**
3. 但在：
   - `RTT 50 ms`
   - `RTT 250 ms`
   - 以及一部分更高 loss 的稳定性问题场景
   
   它仍然是有价值的实验旋钮

如果继续，下一步更合理的是：

- 不再直接推动 `idle_or_drain` 进默认
- 而是改做 **更细粒度的 ProbeRtt entry gate**
  - 比如只在 `high_inflight && !app_limited` 时拦截
  - 而不是一刀切成 `idle_or_drain`

## 2026-03-21：更细粒度的 `ProbeRtt` entry 候选：`last_sample_or_drain`

基于上面的覆盖面复测，我继续补了一个更细粒度的实验模式：

- `HY_RS_BBR_PROBE_RTT_ENTRY=last_sample_or_drain`

语义：

- 仍然避免 **高 inflight 且完全 busy** 时硬切进 `ProbeRtt`
- 但如果已经出现：
  - `last_sample_is_app_limited=true`
  - 或 `event_app_limited=true`
  - 或 `sampler_is_app_limited=true`
  - 或已经 drain 到 `bytes_in_flight <= probe_rtt_cwnd`
- 那就允许进入 `ProbeRtt`

也就是：

- 它比 `idle_or_drain` **更容易在“最近确实出现 app-limited 痕迹”时进入 `ProbeRtt`**
- 但又比完全 `legacy` **更不容易在高 inflight busy phase 误进 `ProbeRtt`**

### 复测设置

统一组合：

- `HY_RS_BBR_APP_LIMITED_SOURCE=hybrid`
- `HY_RS_BBR_APP_LIMITED_TARGET_PCT=50`
- `HY_RS_BBR_STARTUP_FULL_BW_GATE=ack_time_exit_ok`
- `HY_RS_BBR_SAMPLER_APP_LIMITED_EXIT=ack_time_exit_ok`

对照三组：

1. `combo_current`
   - 不设 `HY_RS_BBR_PROBE_RTT_ENTRY`
   - 即当前 `legacy`
2. `combo_probe_rtt_idle_or_drain`
   - `HY_RS_BBR_PROBE_RTT_ENTRY=idle_or_drain`
3. `combo_probe_rtt_last_sample_or_drain`
   - `HY_RS_BBR_PROBE_RTT_ENTRY=last_sample_or_drain`

日志：

- `/tmp/hy_probe_rtt_lastsample_20260321/results.json`

### RTT 250 ms / 30% loss

6 个 round 汇总：

- current：
  - mean `119.422269`
  - median `116.409530`
  - min `103.817641`
- idle_or_drain：
  - mean `90.919306`
  - median `112.368970`
  - min `29.672924`
  - `<50 Mbps`: `2/6`
- last_sample_or_drain：
  - mean `126.528823`
  - median `122.656748`
  - min `110.263781`

结论：

- **`last_sample_or_drain` 明显优于 `idle_or_drain`**
- 而且也**优于 current**
- 这组里它把 `idle_or_drain` 出现的低样本尾部直接消掉了

### RTT 500 ms / 5% loss

6 个 round 汇总：

- current：
  - mean `82.933545`
  - median `82.116571`
  - min `65.375135`
- idle_or_drain：
  - mean `81.371146`
  - median `81.327997`
  - min `56.865452`
- last_sample_or_drain：
  - mean `95.676868`
  - median `95.440914`
  - min `79.429968`

结论：

- 在 `idle_or_drain` 之前出现轻微负收益的这个格子里，
  **`last_sample_or_drain` 反而变成了明显正收益**

### RTT 500 ms / 10% loss

6 个 round 汇总：

- current：
  - mean `64.951305`
  - median `61.228776`
  - min `44.639001`
  - `<50 Mbps`: `2/6`
- idle_or_drain：
  - mean `66.391517`
  - median `65.896622`
  - min `49.223984`
  - `<50 Mbps`: `1/6`
- last_sample_or_drain：
  - mean `82.738483`
  - median `76.743910`
  - min `70.438690`
  - `<50 Mbps`: `0/6`

结论：

- 在之前 `idle_or_drain` 有可信负副作用的高 RTT 场景里，
  **`last_sample_or_drain` 也明显优于 current 和 idle_or_drain**

### RTT 250 ms / 0% loss

6 个 round 汇总：

- current：
  - mean `189.027262`
- idle_or_drain：
  - mean `196.092386`
- last_sample_or_drain：
  - mean `193.666053`

结论：

- **没有看到 `0% loss` 明显副作用**
- `last_sample_or_drain` 大体和 current 持平，略偏正

### 额外状态 trace

我又补了两条 `last_sample_or_drain` 的状态 trace：

- `/tmp/hy_probe_rtt_lastsample_trace_20260321/rtt250_loss30.log`
- `/tmp/hy_probe_rtt_lastsample_trace_20260321/rtt500_loss10.log`

trace 里都能看到：

- `detail=enter_probe_rtt`
- 但进入时：
  - `bytes_in_flight` 已经很低
  - `sampler_is_app_limited=true`
  - `last_sample_is_app_limited=true`

例如：

- RTT 250 ms / 30% loss：
  - `round=25`
  - `bytes_in_flight=547`
- RTT 500 ms / 10% loss：
  - `round=15`
  - `bytes_in_flight=38`

这更像说明：

- `last_sample_or_drain` 的方向是对的
- 它不是一味推迟 `ProbeRtt`
- 而是把 `ProbeRtt` entry 调整成：
  - **避开高 inflight busy 误进入**
  - 但在 **最近样本已经出现 app-limited 痕迹** 时仍允许进入

### 当前结论

到这一步，`ProbeRtt` entry 这条线最新判断是：

1. `idle_or_drain` **不是**安全的全局默认
2. `last_sample_or_drain` 目前看是**更强的候选**
3. 它在这轮关键对照里：
   - **优于 current**
   - **也优于 idle_or_drain**
   - 并且**没看到明显 0% loss 副作用**

所以如果继续，下一步最合理的是：

- 继续扩大一点覆盖面复测 `last_sample_or_drain`
- 如果结果还稳，再考虑把它作为新的默认候选

## 2026-03-22 - 保守最终版收口

这一轮按 **release 基线** 做保守收口，只保留证据最硬的默认改动，其余实验旋钮继续保留为 **opt-in**。

### 最终默认决策

保留进默认：

- `HY_RS_ADAPTIVE_GSO_PERSISTENT_ONLY=true`

不保留进默认：

- `HY_RS_BBR_MAX_BW_APP_LIMITED=ack_time_exit_ok`
  - 默认回到 `legacy`
- `HY_RS_BBR_APP_LIMITED_SOURCE=connection`
- `HY_RS_BBR_APP_LIMITED_SOURCE=hybrid`
- `HY_RS_BBR_APP_LIMITED_TARGET_PCT=50`
- `HY_RS_BBR_STARTUP_FULL_BW_GATE=*`
- `HY_RS_BBR_SAMPLER_APP_LIMITED_EXIT=*`
- `HY_RS_BBR_PROBE_RTT_ENTRY=*`
- `HY_RS_BBR_MAX_BW_UPDATE=refresh_current`
- `HY_RS_BBR_STARTUP_GROWTH=1.15`
- `HY_RS_BBR_STARTUP_ROUNDS=8`

### 为什么只保留 adaptive GSO 默认修正

release 对照里，这个结论一直最稳：

- `250 ms RTT / 30% loss / download`
  - final default：`451.01 Mbps`
  - `HY_RS_ADAPTIVE_GSO_PERSISTENT_ONLY=0`：`233.62 Mbps`

也就是说：

- **non-persistent loss 也触发 GSO auto-disable** 这条旧 aggressive 路径，确实会稳定拖垮高 loss 吞吐；
- 把 adaptive GSO 默认限制到 **persistent congestion**，仍然是本轮里最确定应当保留的默认修正。

对照产物：

- `/tmp/hy_final_release_compare_20260322/results.json`

### 为什么把 `HY_RS_BBR_MAX_BW_APP_LIMITED` 默认回退到 `legacy`

在 debug harness 下，`ack_time_exit_ok` 看起来很强；但 release 基线重跑以后，这个结论不再成立。

这轮 direct A/B：

- `250 ms RTT / 30% loss / download`
  - final default (`legacy`)：`451.01 Mbps`
  - `ack_time_exit_ok`：`490.94 Mbps`
- `500 ms RTT / 30% loss / download`
  - final default (`legacy`)：`191.67 Mbps`
  - `ack_time_exit_ok`：`131.59 Mbps`
- `500 ms RTT / 30% loss / duplex`
  - final default (`legacy`)：`337.70 Mbps`
  - `ack_time_exit_ok`：`294.85 Mbps`

结论：

- `ack_time_exit_ok` 在 **250 ms / 30% 单向下载** 上仍然偏正；
- 但在 **500 ms / 30%** 的下载和 duplex 上更差，而且仍有 failure；
- 所以它不适合作为 **保守通用默认**。

### 为什么 `app_limited source` 不进默认

`app_limited source` 的 trace 很有价值，但仍然更适合保留为实验旋钮，而不是默认：

- `HY_RS_BBR_APP_LIMITED_SOURCE=connection`
  - 能明显修掉部分 `500 ms / 30% / duplex` 下的 `Startup + stage=skip + sample starvation`
  - 但也会拖坏不少别的格子

例如这轮 A/B：

- `250 ms / 30% / upload`
  - current：`485.60 Mbps`
  - `connection`：`398.31 Mbps`
- `250 ms / 30% / duplex`
  - current：`742.75 Mbps`
  - `connection`：`664.53 Mbps`
- `500 ms / 30% / download`
  - current：`195.42 Mbps`
  - `connection`：`111.70 Mbps`

结论：

- 它能解释一些极端格子的坏型；
- 但当前证据不足以支持它成为全局默认。

对照产物：

- `/tmp/hy_app_source_connection_ab_20260321/results.json`
- `/tmp/hy_trace_duplex_current_500_30/`
- `/tmp/hy_trace_duplex_legacy_500_30/`
- `/tmp/hy_trace_duplex_connection_500_30/`

### release 验收矩阵（最终默认）

统一条件：

- release server
- 大窗口：
  - `HY_BENCH_STREAM_RWND=268435456`
  - `HY_BENCH_CONN_RWND=536870912`
  - `HY_SERVER_STREAM_RWND=268435456`
  - `HY_SERVER_CONN_RWND=536870912`
- 单向 delay：
  - `125 ms` => RTT `250 ms`
  - `250 ms` => RTT `500 ms`
- `10s x 1`
- 每格 `3` 次

结果摘要：

#### RTT 250 ms

- `0% / download`：mean `853.31 Mbps`
- `10% / download`：mean `569.68 Mbps`
- `30% / download`：mean `484.92 Mbps`
- `0% / upload`：mean `872.90 Mbps`
- `10% / upload`：mean `648.29 Mbps`
- `30% / upload`：mean `511.62 Mbps`
- `30% / duplex`：mean `688.00 Mbps`
  - `duplex_download` mean `497.41 Mbps`
  - `duplex_upload` mean `338.22 Mbps`

#### RTT 500 ms

- `10% / download`：mean `294.91 Mbps`
- `10% / upload`：mean `346.59 Mbps`
- `10% / duplex`：mean `524.51 Mbps`
  - `duplex_download` mean `338.34 Mbps`
  - `duplex_upload` mean `287.94 Mbps`
- `30% / download`：mean `70.97 Mbps`，`1` 次 failure
- `30% / upload`：mean `221.97 Mbps`，`1` 次 failure
- `30% / duplex`：mean `289.02 Mbps`
  - `duplex_download` mean `170.54 Mbps`
  - `duplex_upload` mean `233.47 Mbps`

矩阵产物：

- `/tmp/hy_final_release_matrix_20260322/results.json`

### 当前最终结论

1. **Hysteria release 数据面已经接近 pure QUIC relay，协议层本身不是主瓶颈**
2. **adaptive GSO 默认改为仅在 persistent congestion 时触发**，应保留
3. `BBR app_limited / startup / sampler / probe_rtt` 这条线：
   - 有不少有价值实验结果
   - 但在 **保守通用默认** 这个标准下，还没有比 `legacy` 更稳的新默认
4. 因此本轮保守 final 选择是：
   - **保留 adaptive GSO 默认修正**
   - **其余 BBR 候选继续保留为实验旋钮，不进默认**
