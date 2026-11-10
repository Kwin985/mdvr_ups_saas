# 版本号

平台版本号在**编译时**写进二进制，运行时不做任何网络查询，也不需要额外配置文件。

## 一、版本号从哪来

| 组成 | 来源 | 示例 |
| --- | --- | --- |
| 语义化版本 | `Cargo.toml` 的 `[package] version` | `0.1.0` |
| 提交号 | `build.rs` 执行 `git rev-parse --short=8 HEAD` | `3f9a2c1` |
| 改动标记 | `build.rs` 执行 `git status --porcelain --untracked-files=no`，有已跟踪改动则为脏 | `.dirty` |
| 构建时间 | `build.rs` 取编译那一刻的时间戳 | `2026-10-28 11:20` |

展示形式：`v0.1.0+3f9a2c1`，脏工作区为 `v0.1.0+3f9a2c1.dirty`。

实现位置：`build.rs`（注入 `UPS_GIT_HASH` / `UPS_GIT_DIRTY` / `UPS_BUILD_EPOCH`）
→ `src/version.rs`（`label()` / `built_at()` / `banner()`）。

没有 git 时（从 tarball 构建）提交号退化为 `unknown`，不影响编译与运行。

## 二、在哪能看到

| 位置 | 内容 |
| --- | --- |
| `mdvr_ups --version` | `UPS Sentinel v0.1.0+3f9a2c1 (built 2026-10-28 11:20)` |
| 启动日志首行 | 同上 |
| `GET /healthz` | `{"version": "...", "built_at": "...", ...}` |
| 登录页页脚 | `v0.1.0+3f9a2c1` |
| 侧栏底部（登录后每页） | `v0.1.0+3f9a2c1`（管理员可点，跳系统设置） |
| 系统设置 → 关于平台 | 版本 + 构建时间 |

`/healthz` 免登录，适合接监控或发布后核对：

```bash
curl -s http://<host>:18080/healthz | grep version
```

## 三、发版流程

1. 改 `Cargo.toml` 的 `version`（按语义化版本：功能 +1 minor，修复 +1 patch）；
2. **先提交**（否则二进制会带 `.dirty` 后缀，看着不像正式版）；
3. 在 WSL 里重新编译：
   ```bash
   cargo build --release --features duckdb/bundled
   ```
4. `scp target/release/mdvr_ups root@<host>:/opt/ups/`；
5. 重启服务：`systemctl restart mdvr-ups`；
6. 核对：`curl -s http://<host>:18080/healthz | grep version`。

## 四、几个容易踩的点

- **版本号只跟二进制走**：换了二进制不重启，页面还是旧号；重启了但没换二进制，号也不会变。
- **`cargo build` 会重跑 `build.rs`**：`build.rs` 里注册了 `.git/HEAD` 与当前分支 ref 的
  `rerun-if-changed`，所以提交后重编会自动更新提交号，不用 `touch`。
- **`.dirty` 说明这个二进制不是你提交的那份代码**：线上看到 `.dirty` 就该重编一次再发。
- 平台版本（本文件）与 **UPS 固件版本**是两回事：固件版本来自设备上报的 `7E 10 00 72 62 7E`
  应答，存在 `devices.firmware_version`，两者别混着说。
