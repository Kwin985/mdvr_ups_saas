# 阿里云部署 Runbook —— UPS Sentinel

> 目标实例：阿里云 ECS `ecs.e-c1m1.large`（2 核 / 2 GiB / Ubuntu 26.04 64 位 /  
> 40 GiB ESSD）。公网 IP `101.37.205.210`，安全组已放行 `1688/tcp`、`18080/tcp`。
>
> 本文件是在该实例上从零拉起服务的可复制步骤。**所有命令在 Ubuntu 实例上执行**  
> （通过 `ssh root@101.37.205.210`）。

## 0. 上线前两个建议（非阻塞）

1. **把公网 IP 转为弹性公网 IP（EIP）。** 控制台实例页有「转换为弹性公网 IP」按钮。  
   MDVR 设备把连接目标 IP 写死，普通公网 IP 在实例停止/变配时可能变动，转 EIP 后稳定解耦。
2. **1688 来源收敛。** 当前安全组 `1688/tcp` 对 `0.0.0.0/0` 开放，而 1688 是裸 TCP、零认证。  
   知道 MDVR 出口 IP 后，在控制台把来源缩到该网段。试用验证期先全开无妨。

## 1. SSH 登录

```bash
ssh root@101.37.205.210
# 若用密钥：ssh -i <你的密钥.pem> root@101.37.205.210
```

## 2. 系统依赖 + 4 GB swap（2 GiB 内存编译 DuckDB 必开 swap）

DuckDB 的 bundled 编译在链接阶段很吃内存，2 GiB 直接编会 OOM；开 4 GB swap 即可。

```bash
sudo apt update
sudo apt install -y build-essential pkg-config cmake clang libclang-dev git curl openss
l

sudo fallocate -l 4G /swapfile
sudo chmod 600 /swapfile
sudo mkswap /swapfile
sudo swapon /swapfile
echo '/swapfile none swap sw 0 0' | sudo tee -a /etc/fstab
```

## 3. 安装 Rust 工具链

```bash
curl --pro



to '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y



```

## 4. 拉代码并编译（bundled DuckDB）

```bash
git clone <你的仓库地址> mdvr_ups && cd mdvr_ups
cargo build --release --features duckdb/bundled
```

- 编译约 **10–25 分钟**（受 swap 速度影响）。产物：`target/release/mdvr_ups`。
- 不想在 2 GiB 小机上编？可在配置更高的机器上交叉编译  
  `x86_64-unknown-linux-gnu` 后 `scp` 上去（Rust 二进制基本静态，拷过去就能跑）。

## 5. 配置运行环境变量（随机密钥！别用默认值）

```bash
mkdir -p /opt/ups/data

export UPS_JWT_SECRET=$(openssl rand -hex 32)
export UPS_ADMIN_PASSWORD=$(openssl rand -base64 18)
export UPS_DB_PATH=/opt/ups/data/ups.duckdb
export UPS_DB_MEMORY_LIMIT_MB=256
export UPS_DB_THREADS=1
# 端口默认 1688 / 18080 即可；要改：
# export UPS_INGEST_ADDR=0.0.0.0:1688
# export UPS_WEB_ADDR=0.0.0.0:18080
```

> `UPS_DB_MEMORY_LIMIT_MB=256` + `UPS_DB_THREADS=1` 在小内存机器上必设，  
> 否则一次大范围聚合查询就能把进程撑爆。

## 6. 先 nohup 起来验证

```bash
nohup ./target/release/mdvr_ups > /opt/ups/ups.log 2>&1 &
sleep 3
tail -f /opt/ups/ups.log
```

确认日志里出现 `1688` 与 `18080` 监听成功、打印 DuckDB 引擎版本。

## 7. 连通性验证

```bash
# Web 控制台（当前是明文 HTTP，仅验证用）
curl -s -o /dev/null -w "%{http_code}\n" http://101.37.205.210:18080/

# 看在线设备 / 状态（登录后在控制台操作，或经 API）
```

- 浏览器打开 `http://101.37.205.210:18080/` 应看到登录页；
- 用上面生成的 `UPS_ADMIN_PASSWORD` 登录（具体账号名见程序默认值）。

## 8. 生产化（建议，非阻塞）

1. **systemd 托管**：写 unit 文件，开机自启 + 崩溃重启；把 §5 的 env 写进  
   `Environment=` 或 `/opt/ups/env` 再 `EnvironmentFile=`。
2. **18080 上 HTTPS**：装 Caddy（`sudo apt install caddy` 或官方源），反代到  
   `127.0.0.1:18080`，并把安全组 18080 来源限到办公 IP；`UPS_WEB_ADDR` 改绑 `127.0.0.1:18080`。
3. **1688 限来源**：安全组把 1688 来源缩到 MDVR 出口网段。
4. **OTA 仍关着**（`OTA_ENABLED` 编译开关默认关），真机验证通过前不要开。

## 排错速查

| 现象                      | 可能原因             | 处理                                     |
| ----------------------- | ---------------- | -------------------------------------- |
| 编译时链接 OOM / 被杀          | 内存不足             | 确认 §2 swap 已 `swapon`；`free -h` 看 swap |
| `cargo build` 卡在 duckdb | bundled 编译中，正常现象 | 等 10–25 分钟；`htop` 看是否在吃 CPU            |
| 外网连不上 1688/18080        | 没绑公网 IP / 安全组没开  | 确认公网 IP + 安全组入站                        |
| 1688 收到连接但无数据           | MDVR 配置的 IP:端口不对 | 核对 MDVR 指向 `101.37.205.210:1688`       |
| 18080 能开但登录失败           | 用的默认密码 / 密钥      | 用 §5 自己生成的 `UPS_ADMIN_PASSWORD`        |
