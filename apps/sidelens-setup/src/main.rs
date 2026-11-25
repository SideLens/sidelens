//! SideLens 安装/卸载程序。
//!
//! # 用法
//!
//! ```text
//! sidelens-setup                          交互式安装（默认装到 %LOCALAPPDATA%\SideLens）
//! sidelens-setup --silent                 静默安装，只输出机器可读进度
//! sidelens-setup --uninstall [--silent]   卸载（--silent 跳过确认）
//! sidelens-setup --dry-run                只打印计划，不落盘
//! sidelens-setup --self-test              在临时目录里跑完整装/升/卸，不碰系统
//! sidelens-setup --install-dir <目录>     自定义安装根（测试与绿色版用）
//! ```
//!
//! # 退出码
//!
//! | 码 | 含义 |
//! | --- | --- |
//! | 0 | 成功 |
//! | 2 | 参数错误 |
//! | 3 | 清单缺失或不合格 |
//! | 4 | 执行失败（复制/删除/注册表） |
//! | 5 | 自测失败 |
//!
//! 退出码是 `tools/verify-release.ps1` 唯一的判据来源 —— 所以这里不能
//! 用 `std::process::exit(1)` 一把梭：脚本要能区分「清单坏了」和「盘写不了」。

#![allow(unsafe_code)]

mod selftest;

// 落盘动作与注册表来自 sidelens-deploy —— 与 sidelensd 的应用内更新共用同一份实现，
// 避免出现「安装器装出来的目录更新器写不了、或更新器写出来的目录安装器卸不干净」。
use sidelens_deploy::{install, plan, registry};

use std::path::PathBuf;

use anyhow::{Context, Result};
use sidelens_update::layout::{uninstall_values, InstalledRecord, Layout};
use sidelens_update::manifest::Manifest;
use sidelens_update::{decide, Decision, Version};

/// 退出码
const EXIT_OK: i32 = 0;
const EXIT_ARGS: i32 = 2;
const EXIT_MANIFEST: i32 = 3;
const EXIT_APPLY: i32 = 4;
const EXIT_SELFTEST: i32 = 5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Install,
    Uninstall,
    DryRun,
    SelfTest,
}

#[derive(Debug, Clone)]
struct Options {
    mode: Mode,
    silent: bool,
    install_dir: Option<PathBuf>,
    /// 卸载时保留用户数据（backups / updates）
    keep_data: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            mode: Mode::Install,
            silent: false,
            install_dir: None,
            keep_data: false,
        }
    }
}

fn main() {
    let code = match run() {
        Ok(code) => code,
        Err(error) => {
            eprintln!("[setup] 失败：{error:#}");
            EXIT_APPLY
        }
    };
    std::process::exit(code);
}

fn run() -> Result<i32> {
    let options = match parse_args(std::env::args().skip(1).collect()) {
        Ok(options) => options,
        Err(error) => {
            eprintln!("[setup] 参数错误：{error}");
            eprintln!("[setup] 用法见 sidelens-setup --help");
            return Ok(EXIT_ARGS);
        }
    };

    if options.mode == Mode::SelfTest {
        let report = selftest::run();
        print!("{}", report.render());
        let _ = std::fs::write(selftest::report_path(), report.render());
        return Ok(if report.passed() {
            EXIT_OK
        } else {
            EXIT_SELFTEST
        });
    }

    let layout = Layout::new(match &options.install_dir {
        Some(dir) => dir.clone(),
        None => default_install_root()?,
    });

    match options.mode {
        Mode::Uninstall => uninstall(&options, &layout),
        Mode::Install | Mode::DryRun => install(&options, &layout),
        Mode::SelfTest => unreachable!("自测已在上方返回"),
    }
}

fn parse_args(args: Vec<String>) -> std::result::Result<Options, String> {
    let mut options = Options::default();
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "--silent" | "-s" => options.silent = true,
            "--uninstall" | "/uninstall" => options.mode = Mode::Uninstall,
            "--dry-run" => options.mode = Mode::DryRun,
            "--self-test" => options.mode = Mode::SelfTest,
            "--keep-data" => options.keep_data = true,
            "--install-dir" => {
                index += 1;
                let value = args
                    .get(index)
                    .ok_or_else(|| "--install-dir 后需要目录".to_string())?;
                options.install_dir = Some(PathBuf::from(value));
            }
            "--help" | "-h" | "/?" => {
                print_usage();
                std::process::exit(EXIT_OK);
            }
            other => return Err(format!("未知参数 {other}")),
        }
        index += 1;
    }
    if options.mode == Mode::Uninstall && options.install_dir.is_none() {
        // 卸载时允许从 installed.json 反查安装根；这里不报错，交由 uninstall() 处理。
    }
    Ok(options)
}

fn print_usage() {
    println!("SideLens 安装程序");
    println!();
    println!("  sidelens-setup                     交互式安装");
    println!("  sidelens-setup --silent            静默安装");
    println!("  sidelens-setup --uninstall         卸载");
    println!("  sidelens-setup --uninstall --silent --keep-data   卸载但保留备份与更新缓存");
    println!("  sidelens-setup --dry-run           只打印计划");
    println!("  sidelens-setup --self-test         沙箱内自测（不碰系统）");
    println!("  sidelens-setup --install-dir DIR   自定义安装根");
}

/// `%LOCALAPPDATA%\SideLens`
fn default_install_root() -> Result<PathBuf> {
    let local = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("XDG_DATA_HOME").map(PathBuf::from))
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local").join("share"))
        })
        .context("拿不到 LOCALAPPDATA，请用 --install-dir 指定安装目录")?;
    Ok(Layout::default_root(&local))
}

/// 发布包目录：安装器 exe 所在目录
fn source_dir() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("拿不到安装器自身路径")?;
    exe.parent()
        .map(PathBuf::from)
        .context("安装器路径没有父目录")
}

fn load_manifest(dir: &std::path::Path) -> Result<Manifest, i32> {
    let path = dir.join(sidelens_update::MANIFEST_FILE);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) => {
            eprintln!("[setup] 读不到清单 {path:?}：{e}");
            eprintln!("[setup] 请把 sidelens-setup.exe 放在发布包目录里，或先运行 tools/package-release.ps1");
            return Err(EXIT_MANIFEST);
        }
    };
    match Manifest::parse(&text) {
        Ok(manifest) => Ok(manifest),
        Err(e) => {
            eprintln!("[setup] 清单不合格：{e}");
            Err(EXIT_MANIFEST)
        }
    }
}

fn install(options: &Options, layout: &Layout) -> Result<i32> {
    let source = source_dir()?;
    let manifest = match load_manifest(&source) {
        Ok(manifest) => manifest,
        Err(code) => return Ok(code),
    };

    // 已安装版本：有 installed.json 就读，读到坏值按"没装过"处理并提示。
    let previous = read_installed(layout);
    if let Some((record, _)) = previous.as_ref() {
        match decide(&record.version.to_string(), &manifest) {
            Decision::UpToDate { .. } => {
                println!(
                    "[setup] 已安装 {}，与发布包 {} 相同，无需更新",
                    record.version, manifest.version
                );
                if options.mode == Mode::DryRun {
                    return Ok(EXIT_OK);
                }
                // 交互式重装仍是合法动作（修复损坏的安装），继续往下走。
            }
            Decision::UnsupportedManifest { .. } | Decision::InvalidVersion { .. } => {
                let decision = decide(&record.version.to_string(), &manifest);
                eprintln!("[setup] {}", decision.message());
                return Ok(EXIT_MANIFEST);
            }
            Decision::ReinstallRequired { .. } => {
                let decision = decide(&record.version.to_string(), &manifest);
                eprintln!("[setup] {}", decision.message());
                return Ok(EXIT_MANIFEST);
            }
            Decision::UpdateAvailable { .. } => {
                println!(
                    "[setup] {}",
                    decide(&record.version.to_string(), &manifest).message()
                );
            }
        }
    }

    let installed_at = timestamp();
    let mut record = InstalledRecord::new(manifest.version, layout, installed_at);
    record.registered_dshow = false;
    let plan = match plan::plan_install(layout, &source, &manifest, record.to_json()) {
        Ok(plan) => plan,
        Err(e) => {
            eprintln!("[setup] {e}");
            return Ok(EXIT_MANIFEST);
        }
    };

    println!("[setup] 安装到 {}", layout.root().display());
    println!(
        "[setup] 版本 {}（{} 个文件）",
        manifest.version,
        manifest.files.len()
    );

    if options.mode == Mode::DryRun {
        for action in &plan.files {
            match action {
                plan::FileAction::Copy { from, to } => println!("  复制 {from:?} → {to:?}"),
                plan::FileAction::Write { to, .. } => println!("  写入 {to:?}"),
                plan::FileAction::RemoveDir { path } => println!("  删除 {path:?}"),
            }
        }
        println!("[setup] --dry-run：未做任何改动");
        return Ok(EXIT_OK);
    }

    let previous_version = previous.as_ref().map(|(r, _)| r.version);
    let report = match install::apply(&plan, layout, previous_version) {
        Ok(report) => report,
        Err(e) => {
            eprintln!("[setup] 安装失败：{e}");
            return Ok(EXIT_APPLY);
        }
    };
    println!("[setup] 文件就位：{}", report.summary());

    // 安装目录自校验：不信任自己刚写的文件，重新按清单算一遍哈希。
    let failures = sidelens_update::verify_tree(layout.root(), &manifest.files);
    if !failures.is_empty() {
        eprintln!("[setup] 安装后校验失败：{failures:?}");
        return Ok(EXIT_APPLY);
    }
    println!("[setup] 校验通过：{} 个文件哈希一致", manifest.files.len());

    // DirectShow：有管理员就注册，没有就降级并在记录里标记。
    let mut registered = false;
    if let Some(dll) = plan.register_dshow.as_ref() {
        if registry::is_elevated() {
            match registry::register_dshow(dll) {
                Ok(()) => {
                    registered = true;
                    println!("[setup] DirectShow 滤镜已注册：{}", dll.display());
                }
                Err(e) => eprintln!("[setup] DirectShow 滤镜注册失败（可稍后用管理员重试）：{e}"),
            }
        } else {
            eprintln!("[setup] 未提权：跳过 DirectShow 滤镜注册");
            eprintln!("[setup]   需要虚拟摄像头时，用管理员终端运行：");
            eprintln!("[setup]   sidelens-setup --register-vcam");
        }
    }

    // 把真实结果写回 installed.json（前面那一步写的是占位值）。
    record.registered_dshow = registered;
    let record_path = layout.installed();
    if let Err(e) = std::fs::write(&record_path, record.to_json()) {
        eprintln!("[setup] 回写 {} 失败：{e}", record_path.display());
        return Ok(EXIT_APPLY);
    }

    // 卸载项：让「应用和功能」能看到 SideLens。
    let setup_exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("sidelens-setup.exe"));
    let values = uninstall_values(&record, &setup_exe);
    match registry::write_uninstall(&values) {
        Ok(()) => println!("[setup] 已在「应用和功能」登记卸载项"),
        Err(e) => eprintln!("[setup] 卸载项写入失败（不影响使用）：{e}"),
    }

    if !options.silent {
        println!();
        println!("安装完成。");
        println!("  启动：{}", layout.exe().display());
        println!("  卸载：{} --uninstall", setup_exe.display());
    } else {
        println!("[setup] SILENT-OK {}", manifest.version);
    }
    Ok(EXIT_OK)
}

fn uninstall(options: &Options, layout: &Layout) -> Result<i32> {
    // 优先用 installed.json 里记的安装根：用户可能把安装目录挪过。
    let installed = read_installed(layout);
    let (record, record_root) = match installed {
        Some(pair) => pair,
        None => {
            if layout.root().exists() {
                eprintln!(
                    "[setup] 目录 {} 存在但没有 installed.json，按该目录卸载",
                    layout.root().display()
                );
                (
                    InstalledRecord::new(Version::new(0, 0, 0), layout, String::new()),
                    layout.root().to_path_buf(),
                )
            } else {
                println!(
                    "[setup] 没有安装记录（{}），无需卸载",
                    layout.root().display()
                );
                return Ok(EXIT_OK);
            }
        }
    };
    let effective = if record.install_root.is_empty() {
        record_root
    } else {
        PathBuf::from(&record.install_root)
    };
    let layout = Layout::new(effective);

    // 清单可能已经被改坏；卸载不依赖它，只在有它时多一步反注册。
    let manifest = std::fs::read_to_string(layout.manifest())
        .ok()
        .and_then(|text| Manifest::parse(&text).ok());

    let plan = match plan::plan_uninstall(&layout, manifest.as_ref()) {
        Ok(plan) => plan,
        Err(e) => {
            eprintln!("[setup] {e}");
            return Ok(EXIT_ARGS);
        }
    };

    println!(
        "[setup] 卸载 {}（版本 {}）",
        layout.root().display(),
        record.version
    );

    if options.mode == Mode::DryRun {
        for action in &plan.files {
            if let plan::FileAction::RemoveDir { path } = action {
                println!("  删除 {path:?}");
            }
        }
        println!("[setup] --dry-run：未做任何改动");
        return Ok(EXIT_OK);
    }

    if let Some(dll) = plan.unregister_dshow.as_ref() {
        if registry::is_elevated() {
            match registry::unregister_dshow(dll) {
                Ok(()) => println!("[setup] DirectShow 滤镜已反注册"),
                Err(e) => eprintln!("[setup] DirectShow 反注册失败（可能本来就没注册）：{e}"),
            }
        } else if record.registered_dshow {
            eprintln!("[setup] 该安装注册过 DirectShow 滤镜，但当前未提权，无法反注册。");
            eprintln!(
                "[setup]   请用管理员终端运行 regsvr32 /u \"{}\"",
                dll.display()
            );
        }
    }

    // 保留用户数据时先把两个目录挪出来
    let stash = if options.keep_data {
        let target = std::env::temp_dir().join(format!("sidelens-keepdata-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&target);
        for name in [sidelens_update::BACKUPS_DIR, sidelens_update::UPDATES_DIR] {
            let from = layout.root().join(name);
            if from.exists() {
                let _ = std::fs::rename(&from, target.join(name));
            }
        }
        Some(target)
    } else {
        None
    };

    match install::apply_uninstall(&plan) {
        Ok(report) => println!("[setup] 卸载完成：{}", report.summary()),
        Err(e) => {
            eprintln!("[setup] 卸载未完成：{e}");
            return Ok(EXIT_APPLY);
        }
    }

    if let Err(e) = registry::remove_uninstall() {
        eprintln!("[setup] 删除卸载项失败（不影响文件删除）：{e}");
    } else {
        println!("[setup] 已从「应用和功能」移除");
    }

    if let Some(stash) = stash {
        eprintln!(
            "[setup] --keep-data：备份与更新缓存已保留在 {}",
            stash.display()
        );
    }

    if options.silent {
        println!("[setup] SILENT-UNINSTALL-OK");
    }
    Ok(EXIT_OK)
}

/// 读 `installed.json`；读不到或坏掉都返回 `None`。
fn read_installed(layout: &Layout) -> Option<(InstalledRecord, PathBuf)> {
    let text = std::fs::read_to_string(layout.installed()).ok()?;
    let record = InstalledRecord::parse(&text).ok()?;
    Some((record, layout.root().to_path_buf()))
}

/// ISO-8601 UTC 时间戳（不引第三方 crate）
fn timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_utc(seconds)
}

/// 秒 → `YYYY-MM-DDTHH:MM:SSZ`。用纯整数运算实现，避免拉 chrono。
fn format_utc(seconds: u64) -> String {
    let days = seconds / 86_400;
    let remainder = seconds % 86_400;
    let (hour, minute, second) = (remainder / 3600, (remainder % 3600) / 60, remainder % 60);
    let (year, month, day) = civil_from_days(days as i64);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Howard Hinnant 的 `civil_from_days`
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
    fn 参数解析识别全部模式() {
        let options = parse_args(vec!["--silent".into(), "--dry-run".into()]).unwrap();
        assert_eq!(options.mode, Mode::DryRun);
        assert!(options.silent);

        let options = parse_args(vec!["--uninstall".into(), "--keep-data".into()]).unwrap();
        assert_eq!(options.mode, Mode::Uninstall);
        assert!(options.keep_data);

        let options = parse_args(vec!["--self-test".into()]).unwrap();
        assert_eq!(options.mode, Mode::SelfTest);
    }

    #[test]
    fn 未知参数被拒绝() {
        assert!(parse_args(vec!["--nope".into()]).is_err());
        assert!(parse_args(vec!["--install-dir".into()]).is_err());
    }

    #[test]
    fn 自定义安装目录被采纳() {
        let options = parse_args(vec!["--install-dir".into(), r"D:\tmp\SideLens".into()]).unwrap();
        assert_eq!(options.install_dir, Some(PathBuf::from(r"D:\tmp\SideLens")));
    }

    #[test]
    fn 默认选项是交互式安装() {
        let options = Options::default();
        assert_eq!(options.mode, Mode::Install);
        assert!(!options.silent);
        assert!(options.install_dir.is_none());
    }

    #[test]
    fn 时间戳格式是_iso8601() {
        // 2026-11-26T00:00:00Z = 1795651200
        assert_eq!(format_utc(1_795_651_200), "2026-11-26T00:00:00Z");
        // 纪元零点是 1970-01-01
        assert_eq!(format_utc(0), "1970-01-01T00:00:00Z");
        // 闰年 2 月 29 日
        assert_eq!(format_utc(1_709_164_800), "2024-02-29T00:00:00Z");
    }

    #[test]
    fn 时间戳当前值落在合理区间() {
        let now = timestamp();
        assert!(now.starts_with("20"), "{now}");
        assert!(now.ends_with('Z'));
        assert_eq!(now.len(), 20);
    }
}
