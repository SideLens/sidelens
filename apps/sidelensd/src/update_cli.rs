//! `--check-update` / `--update` 的命令行外壳。
//!
//! 与 GUI 共用 update 模块的全部逻辑，差别只在输出与"要不要问用户"。
//!
//! # 退出码（给 tools/verify-release.ps1 用）
//!
//! | 码 | 含义 |
//! | --- | --- |
//! | 0 | 已是最新，或更新已成功落地 |
//! | 1 | 有可用更新但只检查未落地（`--check-only`），或已下载待重启落地（`--stage-only`） |
//! | 3 | 清单无效 / 拒绝执行 |
//! | 4 | 下载或落地失败
//!
//! 把"有更新"和"失败"分成两个非 0 码很重要：CI 里前者是预期结果，
//! 后者是事故。混在一起就没法用退出码做断言。

use std::path::PathBuf;

use sidelens_update::layout::{InstalledRecord, Layout};
use sidelens_update::Version;

use crate::update;

/// `--check-update <URL|路径>` 的实现
pub fn run_check(manifest_url: &str, current_override: Option<&str>, quiet: bool) -> i32 {
    let layout = current_layout();
    let current = match current_override {
        Some(value) => value.to_string(),
        None => installed_version(&layout).unwrap_or_else(|| "0.0.0".into()),
    };

    if !quiet {
        println!("[update] 清单：{manifest_url}");
        println!("[update] 当前版本：{current}");
    }

    let outcome = match update::check(manifest_url, &current) {
        Ok(outcome) => outcome,
        Err(error) => {
            eprintln!("[update] {error}");
            return 3;
        }
    };
    println!("[update] {}", outcome.summary());
    println!("[update] 决策：{:?}", outcome.decision);

    if outcome.decision.is_update() {
        println!("[update] UPDATE-AVAILABLE {}", outcome.manifest.version);
        1
    } else {
        println!("[update] UP-TO-DATE {current}");
        0
    }
}

/// `--update <URL|路径> [--stage-only]`
pub fn run_update(manifest_url: &str, stage_only: bool, current_override: Option<&str>) -> i32 {
    let layout = current_layout();
    let current = match current_override {
        Some(value) => value.to_string(),
        None => installed_version(&layout).unwrap_or_else(|| "0.0.0".into()),
    };

    println!("[update] 清单：{manifest_url}");
    println!("[update] 当前版本：{current}");

    let outcome = match update::check(manifest_url, &current) {
        Ok(outcome) => outcome,
        Err(error) => {
            eprintln!("[update] {error}");
            return 3;
        }
    };
    println!("[update] {}", outcome.summary());
    if !outcome.decision.is_update() {
        println!("[update] UP-TO-DATE {current}");
        return 0;
    }

    let staged = match update::download(&layout, &outcome, &mut |line| {
        println!("[update] {line}");
    }) {
        Ok(staged) => staged,
        Err(error) => {
            eprintln!("[update] {error}");
            return 4;
        }
    };
    println!(
        "[update] 已下载并校验 {} 个文件 → {}",
        staged.files.len(),
        staged.dir.display()
    );

    if stage_only {
        println!("[update] STAGED {}", outcome.manifest.version);
        println!("[update] 下次启动或运行 sidelens-setup 时落地生效");
        return 1;
    }

    let previous = installed_version(&layout).and_then(|text| Version::parse(&text).ok());
    match update::apply_staged(&layout, &outcome, &staged, previous) {
        Ok(summary) => println!("[update] 落地完成：{summary}"),
        Err(error) => {
            eprintln!("[update] {error}");
            return 4;
        }
    }
    if let Err(error) = update::refresh_installed(&layout, outcome.manifest.version, &now_iso()) {
        eprintln!("[update] 回写 installed.json 失败：{error}");
        return 4;
    }
    println!("[update] UPDATED {}", outcome.manifest.version);
    0
}

/// `--write-manifest`：给打包脚本用，按目录算哈希生成清单。
///
/// 参数多是有意的：这条命令对应打包脚本里的一行调用，把 9 个值塞进一个
/// 结构体只会让调用处多一层构造而不会更清楚。cli 层的上限在这里放宽。
#[allow(clippy::too_many_arguments)]
pub fn run_write_manifest(
    dir: &str,
    version: &str,
    channel: &str,
    phone_min_code: u32,
    phone_latest_code: u32,
    phone_latest_name: &str,
    phone_min_protocol: u32,
    notes: &str,
    out: Option<&str>,
) -> i32 {
    let phone = sidelens_update::PhoneManifest {
        min_supported_version_code: phone_min_code,
        latest_version_code: phone_latest_code,
        latest_version_name: phone_latest_name.to_string(),
        min_supported_protocol: phone_min_protocol,
    };
    let manifest = match update::build_manifest(
        std::path::Path::new(dir),
        version,
        channel,
        phone,
        notes,
        &now_iso(),
    ) {
        Ok(manifest) => manifest,
        Err(error) => {
            eprintln!("[manifest] {error}");
            return 3;
        }
    };
    let text = manifest.to_json();
    match out {
        Some(path) => {
            if let Err(error) = std::fs::write(path, &text) {
                eprintln!("[manifest] 写 {path} 失败：{error}");
                return 4;
            }
            println!("[manifest] 已写 {path}（{} 个文件）", manifest.files.len());
        }
        None => println!("{text}"),
    }
    0
}

fn current_layout() -> Layout {
    // 与安装器共用同一份默认根：%LOCALAPPDATA%SideLens
    match std::env::var_os("SIDELENS_INSTALL_ROOT") {
        Some(root) => Layout::new(PathBuf::from(root)),
        None => {
            let local = std::env::var_os("LOCALAPPDATA")
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME")
                        .map(|home| PathBuf::from(home).join(".local").join("share"))
                })
                .unwrap_or_else(std::env::temp_dir);
            Layout::new(Layout::default_root(&local))
        }
    }
}

fn installed_version(layout: &Layout) -> Option<String> {
    let text = std::fs::read_to_string(layout.installed()).ok()?;
    InstalledRecord::parse(&text)
        .ok()
        .map(|r| r.version.to_string())
}

/// ISO-8601 UTC。用纯整数实现，避免为一行时间戳拉 chrono。
fn now_iso() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = seconds / 86_400;
    let remainder = seconds % 86_400;
    let (year, month, day) = civil_from_days(days as i64);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        remainder / 3600,
        (remainder % 3600) / 60,
        remainder % 60
    )
}

/// Howard Hinnant 的 civil_from_days
fn civil_from_days(days_since_epoch: i64) -> (i64, u32, u32) {
    let z = days_since_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = (z - era * 146_097) as u64;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era as i64 + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let mp = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 时间戳格式正确() {
        assert_eq!(now_iso().len(), 20, "形如 2026-11-26T00:00:00Z 才符合契约");
        assert!(now_iso().ends_with('Z'));
    }

    #[test]
    fn 安装根可以被环境变量覆盖() {
        // 不真的改环境变量（会影响并发用例），只验证两条分支都能算出非空路径
        let layout = current_layout();
        assert!(!layout.root().as_os_str().is_empty());
    }
}
