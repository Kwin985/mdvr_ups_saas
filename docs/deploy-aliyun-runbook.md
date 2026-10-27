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
# 若用密钥：ssh -i ~/aliyun.pem root@101.37.205.210
```

### 1b. 报 `Permission denied (publickey)` 怎么办

说明实例只接受公钥登录（用密钥对创建，或 root 密码登录被禁用）。三选一：

**A. 有创建实例时下载的 `.pem` 密钥（最快）**

```bash
# WSL 里：把 Windows 上的 pem 拷进 WSL 并收紧权限（WSL 下 600 否则 ssh 拒绝使用）
cp /mnt/c/Users/<你>/Downloads/<你的密钥>.pem ~/aliyun.pem
chmod 600 ~/aliyun.pem
ssh -i ~/aliyun.pem root@101.37.205.210
scp -i ~/aliyun.pem target/release/mdvr_ups root@101.37.205.210:/root/
```

**B. 没有 pem：在 WSL 生成密钥，通过阿里云控制台"远程连接/Workbench"把公钥贴进去**

```bash
# WSL 里生成
ssh-keygen -t ed25519 -N "" -f ~/.ssh/id_ed25519
cat ~/.ssh/id_ed25519.pub          # 复制这一整行
```

然后在控制台用 Workbench/VNC 登录实例，执行：

```bash
mkdir -p ~/.ssh && chmod 700 ~/.ssh
echo '<粘贴刚才复制的公钥>' >> ~/.ssh/authorized_keys
chmod 600 ~/.ssh/authorized_keys
```

回到 WSL 即可免密 `scp target/release/mdvr_ups root@101.37.205.210:/root/`。

> 追加后务必校验：`ls -ld ~/.ssh` 为 `700`、`ls -l ~/.ssh/authorized_keys` 为 `600` 且属主 root；
> 再从 WSL `ssh root@101.37.205.210` 测试。仍失败就看
> `grep -Ei '^(PubkeyAuthentication|PermitRootLogin)' /etc/ssh/sshd_config` 与 `tail -f /var/log/auth.log`。

**C. 改用密码登录（控制台 Workbench 登录后）**

```bash
passwd root
sed -i 's/^#\?PasswordAuthentication.*/PasswordAuthentication yes/' /etc/ssh/sshd_config
sed -i 's/^#\?PermitRootLogin.*/PermitRootLogin yes/' /etc/ssh/sshd_config
systemctl restart ssh
```

> 注：pem 只在创建实例时能下载。丢了就在控制台「密钥对」里重置/重新绑定（无需重装系统）。

## 2. 系统依赖 + 4 GB swap（2 GiB 内存编译 DuckDB 必开 swap）

DuckDB 的 bundled 编译在链接阶段很吃内存，2 GiB 直接编会 OOM；开 4 GB swap 即可。

```bash
sudo apt update
sudo apt install -y build-essential pkg-config cmake clang libclang-dev git curl openssl

sudo fallocate -l 4G /swapfile
sudo chmod 600 /swapfile
sudo mkswap /swapfile
sudo swapon /swapfile
echo '/swapfile none swap sw 0 0' | sudo tee -a /etc/fstab
```

## 3. 安装 Rust 工具链

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
```

## 4. 拉代码并编译（bundled DuckDB）

```bash
git clone https://github.com/Kwin985/mdvr_ups_saas.git mdvr_ups && cd mdvr_ups
cargo build --release --features duckdb/bundled
```

- 编译约 **10–25 分钟**（受 swap 速度影响）。产物：`target/release/mdvr_ups`。

### 4b. 替代：在 WSL 编译后上传（更快，推荐）

实例只有 2 vCPU，首次编译要 20–40 分钟。WSL 是原生 Linux，编出来的
`x86_64-unknown-linux-gnu` 二进制直接兼容，且编译快得多：

```bash
# --- 全部在 WSL 里执行 ---
# 1) 依赖 + Rust（已有可跳过）
sudo apt update && sudo apt install -y build-essential pkg-config cmake clang libclang-dev git curl
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"

# 2) 代码放 WSL 原生文件系统（别在 /mnt/d/... 上编译，9P 文件系统慢一个数量级）
git clone https://github.com/Kwin985/mdvr_ups_saas.git ~/mdvr_ups && cd ~/mdvr_ups

# 3) 编译（WSL 核多内存大，约 5–10 分钟）
cargo build --release --features duckdb/bundled

# 4) 上传到实例（⚠️ 必须连 static/ 一起传，否则页面无样式）
scp target/release/mdvr_ups root@101.37.205.210:/opt/ups/
scp -r static root@101.37.205.210:/opt/ups/
```

实例侧收到后只需：

```bash
chmod +x /opt/ups/mdvr_ups
```

兼容性说明：Rust 二进制默认动态链接 glibc，**旧版 Ubuntu 编出的二进制能在新版上跑**
（glibc 向后兼容），WSL 的 Ubuntu ≤ 26.04 即可；不放心可跑
`ldd target/release/mdvr_ups` 确认依赖都正常解析。
走这条路的话，实例上**不再需要**装 Rust 工具链和 swap，§2–§4 全部可跳过，
直接做 §5；实例上若已在编译，可放心 Ctrl+C 停掉省内存。

## 5. 配置运行环境变量（随机密钥！别用默认值）

```bash
mkdir -p /opt/ups/data

export UPS_JWT_SECRET=$(openssl rand -hex 32)
export UPS_ADMIN_PASSWORD=$(openssl rand -base64 18)
export UPS_DB_PATH=/opt/ups/data/ups.duckdb
export UPS_DB_MEMORY_LIMIT_MB=256
export UPS_DB_THREADS=1
export UPS_STATIC_DIR=/opt/ups/static   # 静态资源目录（CSS/JS/图片），缺了页面会无样式
# 端口默认 1688 / 18080 即可；要改：
# export UPS_INGEST_ADDR=0.0.0.0:1688
# export UPS_WEB_ADDR=0.0.0.0:18080
```

> `UPS_DB_MEMORY_LIMIT_MB=256` + `UPS_DB_THREADS=1` 在小内存机器上必设，  
> 否则一次大范围聚合查询就能把进程撑爆。

## 6. 固化密钥 + 先 nohup 起来验证

> ⚠️ `export` 只活在当前终端，关掉即丢；`UPS_ADMIN_PASSWORD` 是随机值，丢了就登不进控制台。
> **先写进 `/opt/ups/env` 再启动。**

```bash
cat > /opt/ups/env <<EOF
UPS_JWT_SECRET=$UPS_JWT_SECRET
UPS_ADMIN_PASSWORD=$UPS_ADMIN_PASSWORD
UPS_DB_PATH=/opt/ups/data/ups.duckdb
UPS_DB_MEMORY_LIMIT_MB=256
UPS_DB_THREADS=1
EOF
chmod 600 /opt/ups/env
cat /opt/ups/env          # 记下 UPS_ADMIN_PASSWORD，登录要用

# 用 env 文件启动（保证变量齐全）
cd /opt/ups
set -a; . /opt/ups/env; set +a
nohup /opt/ups/mdvr_ups > /opt/ups/ups.log 2>&1 &
sleep 3
tail -n 50 /opt/ups/ups.log
```

确认日志里出现 `1688` 与 `18080` 监听成功、打印 DuckDB 引擎版本。

## 7. 连通性验证

```bash
# 本机监听是否起来
ss -lntp | grep -E '1688|18080'

# 本机自测
curl -s -o /dev/null -w "%{http_code}\n" http://127.0.0.1:18080/

# 外网自测（从 WSL/本机跑，验证安全组与公网 IP）
curl -s -o /dev/null -w "%{http_code}\n" http://101.37.205.210:18080/
```

- 浏览器打开 `http://101.37.205.210:18080/` 应看到登录页；
- 用 `/opt/ups/env` 里的 `UPS_ADMIN_PASSWORD` 登录（具体账号名见程序默认值）。

## 8. 生产化（建议，非阻塞）

### 8.1 systemd 托管（开机自启 + 崩溃重启）

先停掉 nohup 那个实例，再交给 systemd：

```bash
pkill -f /opt/ups/mdvr_ups

cat > /etc/systemd/system/mdvr-ups.service <<'EOF'
[Unit]
Description=UPS Sentinel
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
WorkingDirectory=/opt/ups
EnvironmentFile=/opt/ups/env
ExecStart=/opt/ups/mdvr_ups
Restart=always
RestartSec=3
LimitNOFILE=65535

[Install]
WantedBy=multi-user.target
EOF

systemctl daemon-reload
systemctl enable --now mdvr-ups
systemctl status mdvr-ups --no-pager
journalctl -u mdvr-ups -f
```

### 8.2 其余

2. **18080 上 HTTPS**：装 Caddy（`sudo apt install caddy` 或官方源），反代到  
   `127.0.0.1:18080`，并把安全组 18080 来源限到办公 IP；`UPS_WEB_ADDR` 改绑 `127.0.0.1:18080`。
3. **1688 限来源**：安全组把 1688 来源缩到 MDVR 出口网段。
4. **OTA 仍关着**（`OTA_ENABLED` 编译开关默认关），真机验证通过前不要开。

## 排错速查

| 现象                      | 可能原因             | 处理                                     |
| ----------------------- | ---------------- | -------------------------------------- |
| `Permission denied (publickey)` | 实例用密钥对创建，或 root 禁用密码登录 | 见 §1b：`-i 密钥.pem`，或控制台 Workbench 贴公钥 |
| `scp: /opt/ups/: No such file or directory` | 目标目录不存在 | 先 `mkdir -p /opt/ups`，或先传 `/root/` 再移 |
| 编译时链接 OOM / 被杀          | 内存不足             | 确认 §2 swap 已 `swapon`；`free -h` 看 swap |
| `cargo build` 卡在 duckdb | bundled 编译中，正常现象 | 等 10–25 分钟；`htop` 看是否在吃 CPU            |
| 外网连不上 1688/18080        | 没绑公网 IP / 安全组没开  | 确认公网 IP + 安全组入站                        |
| 1688 收到连接但无数据           | MDVR 配置的 IP:端口不对 | 核对 MDVR 指向 `101.37.205.210:1688`       |
| 18080 能开但登录失败           | 用的默认密码 / 密钥      | 用 §5 自己生成的 `UPS_ADMIN_PASSWORD`        |
| 页面能登录但**完全没有样式** | 缺 `static/` 目录（CSS/JS 404） | `scp -r static root@...:/opt/ups/`；或设 `UPS_STATIC_DIR` |
