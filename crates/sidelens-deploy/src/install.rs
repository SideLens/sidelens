//! 计划执行：复制、原子写、备份、删除。
//!
//! # 三条不变量
//!
//! 1. **每个文件独立原子**：先写 `<name>.tmp`、`sync_all`、再 `rename`。
//!    中途崩只会留下一个 `.tmp`，不会留下半个 exe。
//! 2. **覆盖前先备份**：升级时把旧文件挪进 `backups/<旧版本>`，
//!    最多留 `MAX_BACKUPS` 份。用户回滚只需要拷回来。
//! 3. **失败要能说出来**：任何一步失败都带上路径与原因，安装器据此返回非 0 退出码。
//!
//! 这三条是本模块全部测试的对象；注册表与提权在 `registry` 里，单独测。

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use sidelens_update::layout::{Layout, MAX_BACKUPS};
use sidelens_update::Version;

use crate::plan::{FileAction, InstallPlan, UninstallPlan};

/// 执行结果
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ApplyReport {
    pub copied: Vec<PathBuf>,
    pub written: Vec<PathBuf>,
    pub backed_up: Vec<PathBuf>,
    pub removed: Vec<PathBuf>,
}

impl ApplyReport {
    pub fn summary(&self) -> String {
        format!(
            "复制 {} 个文件、写入 {} 个元数据文件、备份 {} 个旧文件、删除 {} 处",
            self.copied.len(),
            self.written.len(),
            self.backed_up.len(),
            self.removed.len()
        )
    }
}

/// 执行一份计划
pub fn apply(
    plan: &InstallPlan,
    layout: &Layout,
    previous_version: Option<Version>,
) -> Result<ApplyReport, String> {
    let mut report = ApplyReport::default();

    // 升级时先把整目录里已存在的清单文件备份走 —— 备份在覆盖之前，
    // 这样即使后续复制失败，用户手上仍有可回滚的上一版。
    if let Some(version) = previous_version {
        for action in &plan.files {
            let target = match action {
                FileAction::Copy { to, .. } | FileAction::Write { to, .. } => to,
                FileAction::RemoveDir { .. } => continue,
            };
            if !target.is_file() {
                continue;
            }
            let backup_root = layout.backup_dir(&version);
            let relative = target
                .strip_prefix(layout.root())
                .map_err(|_| format!("{target:?} 不在安装根下，拒绝备份"))?;
            let destination = backup_root.join(relative);
            if let Some(parent) = destination.parent() {
                fs::create_dir_all(parent).map_err(|e| format!("建备份目录失败：{e}"))?;
            }
            fs::copy(target, &destination).map_err(|e| format!("备份 {target:?} 失败：{e}"))?;
            report.backed_up.push(destination);
        }
    }

    fs::create_dir_all(layout.root()).map_err(|e| format!("建安装目录失败：{e}"))?;

    for action in &plan.files {
        match action {
            FileAction::Copy { from, to } => {
                if let Some(parent) = to.parent() {
                    fs::create_dir_all(parent)
                        .map_err(|e| format!("建目录 {parent:?} 失败：{e}"))?;
                }
                copy_atomic(from, to)?;
                report.copied.push(to.clone());
            }
            FileAction::Write { to, text } => {
                if let Some(parent) = to.parent() {
                    fs::create_dir_all(parent)
                        .map_err(|e| format!("建目录 {parent:?} 失败：{e}"))?;
                }
                write_atomic(to, text.as_bytes())?;
                report.written.push(to.clone());
            }
            FileAction::RemoveDir { .. } => {
                return Err("安装计划里不该有删除动作".into());
            }
        }
    }

    prune_backups(layout);
    Ok(report)
}

/// 执行卸载计划
pub fn apply_uninstall(plan: &UninstallPlan) -> Result<ApplyReport, String> {
    let mut report = ApplyReport::default();
    for action in &plan.files {
        match action {
            FileAction::RemoveDir { path } => {
                remove_dir_all_robust(path)?;
                report.removed.push(path.clone());
            }
            _ => return Err("卸载计划里只应有删除动作".into()),
        }
    }
    Ok(report)
}

/// `from` → `to`，目标侧先写临时文件再改名
pub fn copy_atomic(from: &Path, to: &Path) -> Result<(), String> {
    let bytes = fs::read(from).map_err(|e| format!("读 {from:?} 失败：{e}"))?;
    write_atomic(to, &bytes)
}

fn write_atomic(to: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = to.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent).map_err(|e| format!("建目录 {parent:?} 失败：{e}"))?;
        }
    }
    let tmp = tmp_path(to);
    {
        let mut file = fs::File::create(&tmp).map_err(|e| format!("创建 {tmp:?} 失败：{e}"))?;
        file.write_all(bytes)
            .map_err(|e| format!("写 {tmp:?} 失败：{e}"))?;
        file.sync_all()
            .map_err(|e| format!("刷盘 {tmp:?} 失败：{e}"))?;
    }
    // Windows 上 rename 到一个已存在的文件会失败（E_EXIST），所以先删目标。
    // 目标被别的进程占用时这里会失败 —— 调用方（GUI）会提示用户先关掉应用。
    if to.exists() {
        fs::remove_file(to)
            .map_err(|e| format!("替换 {to:?} 失败（文件可能正被占用，请先关闭 SideLens）：{e}"))?;
    }
    rename_with_retry(&tmp, to)
}

/// Windows 上 `rename` 会被杀毒软件的实时扫描短暂锁住，返回
/// `ERROR_ACCESS_DENIED` (os error 5) 或 `ERROR_SHARING_VIOLATION` (32)。
///
/// 这不是我们的 bug，但**必须处理**：一次瞬时占用就让整个安装失败，
/// 用户看到的是"装到一半报拒绝访问"。所有 Windows 安装器都做这件重试。
///
/// 退避 4 轮、共约 150ms —— 病毒扫描的锁通常在几十毫秒内释放，
/// 再久就该让用户知道（错误信息里明确说"可能正被占用"）。
fn rename_with_retry(from: &Path, to: &Path) -> Result<(), String> {
    const DELAYS_MS: [u64; 4] = [5, 15, 45, 90];
    let mut last = String::new();
    for (attempt, delay) in DELAYS_MS.iter().enumerate() {
        match fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) => {
                let retryable = matches!(e.raw_os_error(), Some(5) | Some(32) | Some(33));
                last = format!("{e}");
                if !retryable || attempt + 1 == DELAYS_MS.len() {
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(*delay));
            }
        }
    }
    let _ = fs::remove_file(from);
    Err(format!(
        "改名 {from:?} → {to:?} 失败：{last}（文件可能正被占用，请先关闭 SideLens 与杀毒扫描）"
    ))
}

fn tmp_path(to: &Path) -> PathBuf {
    let mut name = to.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".tmp");
    to.with_file_name(name)
}

/// 删目录；目录里文件被占用时记住失败项而不是中途放弃
fn remove_dir_all_robust(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    let mut locked = Vec::new();
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            let child = entry.path();
            if child.is_dir() {
                if let Err(e) = remove_dir_all_robust(&child) {
                    locked.push(format!("{child:?}: {e}"));
                }
            } else if let Err(e) = fs::remove_file(&child) {
                locked.push(format!("{child:?}: {e}"));
            }
        }
    }
    match fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(_) if !locked.is_empty() => {
            Err(format!("卸载未完成，以下文件被占用：{}", locked.join("；")))
        }
        Err(e) => Err(format!("删目录 {path:?} 失败：{e}")),
    }
}

/// 只保留最近 `MAX_BACKUPS` 份备份（按目录修改时间）
pub fn prune_backups(layout: &Layout) {
    let dir = layout.backups_dir();
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    let mut dirs: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let modified = e.metadata().ok()?.modified().ok()?;
            Some((modified, e.path()))
        })
        .collect();
    if dirs.len() <= MAX_BACKUPS {
        return;
    }
    dirs.sort_by_key(|(time, _)| *time);
    // 最新的 MAX_BACKUPS 份留下，其余删掉
    let remove_count = dirs.len() - MAX_BACKUPS;
    for (_, path) in dirs.into_iter().take(remove_count) {
        let _ = fs::remove_dir_all(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::plan_install;
    use sidelens_update::hash::sha256_bytes;
    use sidelens_update::manifest::{Manifest, ManifestFile, PhoneManifest};

    /// 每个用例一个**唯一**目录。
    ///
    /// `cargo test` 默认并发跑同进程内的用例，若两个用例算出同一个目录名，
    /// 它会先 delete 再 create 把对方的文件扫掉 —— 症状是随机的
    /// 「断言失败但单独跑又通过」。踩过一次（src-1 被两个用例共用）。
    fn scratch(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "sidelens-setup-apply-{}-{}-{name}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn manifest_for(files: &[(&str, &[u8])]) -> Manifest {
        Manifest {
            schema: 1,
            product: "SideLens".into(),
            channel: "stable".into(),
            version: Version::new(0, 2, 0),
            released: String::new(),
            min_upgradable_version: Version::new(0, 0, 0),
            files: files
                .iter()
                .map(|(path, bytes)| ManifestFile {
                    path: (*path).to_string(),
                    size: bytes.len() as u64,
                    sha256: sha256_bytes(bytes),
                })
                .collect(),
            phone: PhoneManifest::default(),
            notes: String::new(),
        }
    }

    fn source_with(files: &[(&str, &[u8])]) -> PathBuf {
        let dir = scratch("src");
        for (path, bytes) in files {
            let full = dir.join(path.replace('/', std::path::MAIN_SEPARATOR_STR));
            if let Some(parent) = full.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            fs::write(full, bytes).unwrap();
        }
        dir
    }

    #[test]
    fn 全新安装写出全部文件且内容一致() {
        let source = source_with(&[("sidelensd.exe", b"bin"), ("sdk/a.dll", b"dll")]);
        let layout = Layout::new(scratch("fresh").join("SideLens"));
        let manifest = manifest_for(&[("sidelensd.exe", b"bin"), ("sdk/a.dll", b"dll")]);
        let plan = plan_install(&layout, &source, &manifest, "{\"v\":1}".into()).unwrap();

        let report = apply(&plan, &layout, None).expect("安装应成功");
        assert_eq!(report.copied.len(), 2);
        assert_eq!(report.written.len(), 2);
        assert_eq!(
            fs::read(layout.join_relative("sidelensd.exe")).unwrap(),
            b"bin"
        );
        assert!(layout.manifest().is_file());
        assert!(layout.installed().is_file());
        // 安装后的目录应当通过清单自校验
        assert!(sidelens_update::verify_tree(layout.root(), &manifest.files).is_empty());
    }

    #[test]
    fn 升级时先备份旧版本再覆盖() {
        let source = source_with(&[("sidelensd.exe", b"v2")]);
        let layout = Layout::new(scratch("upgrade").join("SideLens"));
        fs::create_dir_all(layout.root()).unwrap();
        fs::write(layout.exe(), b"v1").unwrap();

        let manifest = manifest_for(&[("sidelensd.exe", b"v2")]);
        let plan = plan_install(&layout, &source, &manifest, "{}".into()).unwrap();
        let report = apply(&plan, &layout, Some(Version::new(0, 1, 0))).expect("升级应成功");

        assert_eq!(report.backed_up.len(), 1);
        let backup = layout
            .backup_dir(&Version::new(0, 1, 0))
            .join("sidelensd.exe");
        assert_eq!(fs::read(&backup).unwrap(), b"v1", "备份里应是旧内容");
        assert_eq!(fs::read(layout.exe()).unwrap(), b"v2", "安装目录应是新内容");
    }

    #[test]
    fn 没有旧版本时不产生备份目录() {
        let source = source_with(&[("sidelensd.exe", b"v1")]);
        let layout = Layout::new(scratch("nobackup").join("SideLens"));
        let manifest = manifest_for(&[("sidelensd.exe", b"v1")]);
        let plan = plan_install(&layout, &source, &manifest, "{}".into()).unwrap();
        apply(&plan, &layout, None).unwrap();
        assert!(!layout.backups_dir().exists());
    }

    #[test]
    fn 原子写不留_tmp_残留() {
        let dir = scratch("tmp");
        let target = dir.join("a.bin");
        write_atomic(&target, b"data").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"data");
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "不该留下临时文件");
    }

    #[test]
    fn 原子写可以覆盖已有文件() {
        let dir = scratch("overwrite");
        let target = dir.join("a.bin");
        fs::write(&target, b"old").unwrap();
        write_atomic(&target, b"new").unwrap();
        assert_eq!(fs::read(&target).unwrap(), b"new");
    }

    #[test]
    fn 目标目录不存在时自动创建() {
        let dir = scratch("mkdir");
        let target = dir.join("deep").join("nested").join("a.bin");
        write_atomic(&target, b"x").unwrap();
        assert!(target.is_file());
    }

    #[test]
    fn 卸载删掉整个安装根() {
        let root = scratch("uninstall").join("SideLens");
        fs::create_dir_all(root.join("sdk")).unwrap();
        fs::write(root.join("sidelensd.exe"), b"x").unwrap();
        fs::write(root.join("sdk").join("a.dll"), b"y").unwrap();

        let layout = Layout::new(&root);
        let plan = crate::plan::plan_uninstall(&layout, None).unwrap();
        let report = apply_uninstall(&plan).unwrap();
        assert_eq!(report.removed.len(), 1);
        assert!(!root.exists(), "安装根应被删除");
    }

    #[test]
    fn 卸载不存在的目录不报错() {
        let root = scratch("uninstall-missing").join("SideLens");
        let layout = Layout::new(&root);
        let plan = crate::plan::plan_uninstall(&layout, None).unwrap();
        assert!(apply_uninstall(&plan).is_ok());
    }

    #[test]
    fn 备份只保留最近若干份() {
        let root = scratch("prune").join("SideLens");
        let layout = Layout::new(&root);
        for i in 0..5u64 {
            let dir = layout.backups_dir().join(format!("0.0.{i}"));
            fs::create_dir_all(&dir).unwrap();
            fs::write(dir.join("x"), b"x").unwrap();
        }
        prune_backups(&layout);
        let count = fs::read_dir(layout.backups_dir()).unwrap().count();
        assert_eq!(count, MAX_BACKUPS);
    }

    #[test]
    fn 复制失败时错误信息带源路径() {
        let dir = scratch("copy-fail");
        let error = copy_atomic(&dir.join("missing.bin"), &dir.join("out.bin")).unwrap_err();
        assert!(error.contains("missing.bin"), "{error}");
    }
}
