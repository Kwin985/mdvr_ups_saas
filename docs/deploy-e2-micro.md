# 部署评估：Google Cloud Always Free（e2-micro）

> **本文的前提是能申请到 Google Cloud。** 若账号/信用卡无法通过（常见于国内卡片），
> 改看 [`deploy-free-tier.md`](deploy-free-tier.md)——那里按「本程序需要公网 IP + 两个
> 入站 TCP 端口」这个硬约束重新筛了一遍免费/试用方案。
> 本文的实测数字（内存/CPU/磁盘/流量）与选哪家厂商无关，两边通用。

> 结论先行：**能跑，而且比预期能带得多**——但前提是开响应压缩、限制 DuckDB 资源、
> 并按设备量调保留期。所有数字来自本仓库的实测（release 构建 + 独立实例 + 临时库）。

## 1. 免费层规格与实测对照

| 资源 | e2-micro 免费额度 | 实测占用（1000 台 / 30s 上报） | 结论 |
| --- | --- | --- | --- |
| vCPU | 0.25 核（共享、可突发） | **0.078 核** | 约 3200 台打满基线 |
| 内存 | 1 GB | **58.5 MB** RSS | 完全够，瓶颈不在内存 |
| 磁盘 | 30 GB HDD | **51.8 KB/台/天** | 180 天保留约 2680 台 |
| 出站流量 | **1 GB/月** | 见第 3 节 | **真正的硬约束** |

内存明细：空载 37 MB，每连接约 20 KB（含 tokio 读写缓冲）。

## 2. 出站流量：最容易被忽略的坑

设备侧几乎不产生出站流量——平台对 `a` 帧只在握手时回一个 8 字节 ACK，入站还不计费。
**流量全花在 Web 控制台上**，因为页面会轮询 `/api/tree`，而该接口把整棵树
（每台设备约 193 字节）重新序列化一遍。

轮询间隔现为 **20 秒**（`static/shell.js` 的 `TREE_POLL_MS`），且页面切到后台标签页时
自动暂停、切回时立即补一次。下表按 1000 台、控制台挂 8 小时/天估算：

| 场景 | 每次轮询 | 每月出站 |
| --- | --- | --- |
| 未压缩 | 194 KB | 约 8.4 GB |
| gzip | 6.8 KB | 约 294 MB |
| **brotli（默认启用）** | **4.0 KB** | **约 169 MB** |

（早期 8 秒间隔时对应 21 GB / 730 MB / 430 MB，间隔放宽后按比例下降 2.5 倍。）

所以 v0.1.0 起默认挂了 `CompressionLayer`（gzip + brotli）。**这是能上免费层的前提**，
不是优化项。压测数据：`/api/tree` 194 KB → 4.0 KB（49×），`/monitor` 203 KB → 6.9 KB。

即便如此，仍建议：控制台不要长期挂在后台；设备量上千时可以把 `TREE_POLL_MS` 再调到 30000。

## 3. 按设备量的推荐配置

| 设备量 | 判断 | 关键配置 |
| --- | --- | --- |
| ≤ 200 台 | 很轻松，CPU <2% | 默认即可 |
| 500 台 | 舒适 | 默认即可 |
| 1000 台 | 可用 | `UPS_RETENTION_DAYS=180`、`UPS_DB_THREADS=1`、`UPS_DB_MEMORY_LIMIT_MB=256` |
| 2000 台 | 接近上限（CPU 0.16 核，磁盘刚够 180 天） | 同上，并把上报周期放宽到 60s |
| 3000 台以上 | **不建议**，换 e2-small 或加数据盘 | — |

设备量上限由 **CPU 与磁盘**共同决定，两者都落在 2500–3000 台；内存要到 2 万台才成为瓶颈。

## 4. 部署清单

### 4.1 环境变量

```ini
UPS_WEB_ADDR=0.0.0.0:18080
UPS_INGEST_ADDR=0.0.0.0:1688
UPS_DB_PATH=/opt/ups-sentinel/data/ups.duckdb
UPS_FIRMWARE_DIR=/opt/ups-sentinel/data/firmware
UPS_RETENTION_DAYS=180
UPS_ADMIN_PASSWORD=<强口令>
UPS_JWT_SECRET=<随机串，openssl rand -hex 32>
UPS_DB_THREADS=1
UPS_DB_MEMORY_LIMIT_MB=256
# UPS_SEED_DEMO 不要设置
```

`UPS_DB_THREADS` / `UPS_DB_MEMORY_LIMIT_MB` 不设时沿用 DuckDB 默认（= 核数、内存 80%），
在 1 GB 机器上默认值会让一次大范围聚合查询直接把进程撑爆，**必须显式限制**。

### 4.2 systemd

```ini
[Unit]
Description=UPS Sentinel
After=network-online.target

[Service]
Type=simple
WorkingDirectory=/opt/ups-sentinel
EnvironmentFile=/opt/ups-sentinel/ups.env
ExecStart=/opt/ups-sentinel/mdvr_ups
Restart=always
RestartSec=3
# 限制 RSS，防止异常查询拖垮整机
MemoryMax=700M

[Install]
WantedBy=multi-user.target
```

### 4.3 GCP 防火墙

```bash
gcloud compute firewall-rules create ups-ingest --allow tcp:1688 --source-ranges 0.0.0.0/0
gcloud compute firewall-rules create ups-web    --allow tcp:18080 --source-ranges <办公网段>/32
```

管理端不要对全网开放。设备侧 1688 必须可达。

### 4.4 构建与分发

本机（Windows）产物不能直接用，需要在 Linux 上构建：

```bash
# 官方预编译的 DuckDB 动态库（版本必须与 Cargo.lock 里的 duckdb crate 对齐，当前 v1.5.6）
wget https://github.com/duckdb/duckdb/releases/download/v1.5.6/libduckdb-linux-amd64.zip
unzip libduckdb-linux-amd64.zip -d /usr/local/lib && ldconfig

export DUCKDB_LIB_DIR=/usr/local/lib DUCKDB_INCLUDE_DIR=/usr/local/include
cargo build --release
```

**不要在 e2-micro 上直接编译**：链接 DuckDB 时 rustc 峰值内存远超 1 GB，几乎必然 OOM。
在 4 GB 以上的机器上构建，把 `mdvr_ups` 与 `libduckdb.so` 一起拷过去；
或者临时加 2 GB swap 再编译。

## 5. 已知限制

* **跨境链路**：e2-micro 只在 us-west1 / us-central1 / us-east1。国内设备每 30s 发 34 字节没问题，
  但 OTA 传固件（几百 KB～几 MB）走跨境会慢且易断，建议 OTA 只在就近节点或局域网执行。
* **临时外部 IP**：实例重启后外部 IP 会变，设备端写死 IP 的场景请预留静态 IP。
* **HDD 磁盘**：30 GB 标准盘的 IOPS 很低，DuckDB checkpoint 期间写放大会变慢；
  实测写入是批量 Append，正常量级下感知不到，但别在上面跑大范围导出。
* **CPU 是共享基线**：0.25 核是长期均值，突发可以更高；一旦长期超用会被限速，
  所以上表按 0.25 核算容量，没有再往上加。
