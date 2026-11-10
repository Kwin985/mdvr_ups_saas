//! 构建期注入版本元信息：短提交号、是否有未提交改动、构建时间戳。
//!
//! 值通过 `cargo:rustc-env` 传给 `src/version.rs` 里的 `env!`，
//! 因此**只有重新编译才会更新** —— 发版前务必用新编的二进制，别复用旧的。
//!
//! 没有 git（比如从 tarball 构建）时提交号退化为 `unknown`，不影响编译。

use std::path::Path;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    // 提交/切换分支后要让本脚本重跑，否则版本号会停在旧提交上。
    // HEAD 里存的是 "ref: refs/heads/main"，提交时该文件内容不变，
    // 所以还要监听它指向的 ref 文件。
    if Path::new(".git/HEAD").exists() {
        println!("cargo:rerun-if-changed=.git/HEAD");
        if let Ok(head) = std::fs::read_to_string(".git/HEAD")
            && let Some(r) = head.trim().strip_prefix("ref: ")
        {
            println!("cargo:rerun-if-changed=.git/{r}");
        }
    }

    let commit = git(&["rev-parse", "--short=8", "HEAD"]).unwrap_or_else(|| "unknown".into());
    // 只看已跟踪文件的改动：target/ 之类的未跟踪产物不算“脏”
    let dirty = git(&["status", "--porcelain", "--untracked-files=no"])
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    let epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    println!("cargo:rustc-env=UPS_GIT_HASH={commit}");
    println!(
        "cargo:rustc-env=UPS_GIT_DIRTY={}",
        if dirty { "1" } else { "0" }
    );
    println!("cargo:rustc-env=UPS_BUILD_EPOCH={epoch}");
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!s.is_empty()).then_some(s)
}
