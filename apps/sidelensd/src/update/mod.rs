//! 应用内更新：检查 → 下载 → 校验 → 原子替换。
//!
//! 契约见 docs/release-manifest.md。判据（该不该更新、版本怎么比）在
//! crates/sidelens-update 里；落盘动作在 crates/sidelens-deploy 里。
//! 这一层只负责**串起来**，以及处理只有运行时才知道的事情：
//! 从哪拿清单、下到哪、什么时候该提示重启。
//!
//! # 为什么更新不就地覆盖正在运行的 exe
//!
//! Windows 允许改写正在运行的 exe（它是「共享删除」打开的），但**不允许**
//! 删除它，而 install.rs 的原子替换是「写 .tmp → 删旧文件 → 改名」。
//! 因此更新分两段：
//!
//! 1. 下载阶段写进 updates/<版本>/，全量校验通过才算成功；
//! 2. 落地阶段由 \`sidelens-setup --apply-staged\` 或用户重启后执行，
//!    此时 sidelensd 可能已退出，文件不再被占用。
//!
//! 无人值守场景（托盘后台）走第 2 种：下载好就提示「重启后生效」，
//! 而不是硬顶着占用去删文件。

pub mod fetch;
pub mod hint;

use std::path::{Path, PathBuf};

use sidelens_update::decision::{decide, Decision};
use sidelens_update::hash::{sha256_file, verify_file, CheckError};
use sidelens_update::layout::Layout;
use sidelens_update::manifest::{Manifest, ManifestFile};
use sidelens_update::Version;

#[derive(Debug)]
pub enum UpdateError {
    Fetch(String),
    Manifest(String),
    /// 下载下来的文件与清单不符
    Corrupt {
        path: String,
        reason: String,
    },
    Io(String),
    Refused(String),
}

impl std::fmt::Display for UpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            UpdateError::Fetch(e) => write!(f, "取回清单/产物失败：{e}"),
            UpdateError::Manifest(e) => write!(f, "清单无效：{e}"),
            UpdateError::Corrupt { path, reason } => {
                write!(f, "\"{path}\" 校验失败：{reason}")
            }
            UpdateError::Io(e) => write!(f, "本地读写失败：{e}"),
            UpdateError::Refused(reason) => write!(f, "拒绝执行更新：{reason}"),
        }
    }
}

impl std::error::Error for UpdateError {}

/// 检查结果：决策 + 拿到手的清单
#[derive(Debug, Clone)]
pub struct CheckOutcome {
    pub decision: Decision,
    pub manifest: Manifest,
    /// 清单最终解析出的地址（相对 URL 已按基准展开），后续下载用它做基准
    pub manifest_url: String,
}

impl CheckOutcome {
    pub fn summary(&self) -> String {
        format!(
            "{}；{} 个文件，合计 {} 字节",
            self.decision.message(),
            self.manifest.files.len(),
            self.manifest.files.iter().map(|f| f.size).sum::<u64>()
        )
    }
}

/// 取回清单并做升级决策。
///
/// current_version 为空时按「没装过」处理（版本 0.0.0）—— 全新安装走这条路。
pub fn check(manifest_url: &str, current_version: &str) -> Result<CheckOutcome, UpdateError> {
    let bytes = fetch::fetch(manifest_url, None).map_err(|e| UpdateError::Fetch(e.to_string()))?;
    let text = String::from_utf8(bytes).map_err(|e| UpdateError::Manifest(e.to_string()))?;
    let manifest = Manifest::parse(&text).map_err(|e| UpdateError::Manifest(e.to_string()))?;
    let current = if current_version.trim().is_empty() {
        "0.0.0"
    } else {
        current_version
    };
    Ok(CheckOutcome {
        decision: decide(current, &manifest),
        manifest,
        manifest_url: manifest_url.to_string(),
    })
}

/// 已下载并校验完的更新
#[derive(Debug, Clone)]
pub struct Staged {
    pub version: Version,
    /// updates/<版本>/
    pub dir: PathBuf,
    pub files: Vec<(String, PathBuf)>,
}

/// 把清单里的每个文件下载到 layout.updates_dir()/<版本>/ 并逐个校验。
///
/// # 失败即丢弃
///
/// 任何一个文件校验不过就清空整个暂存目录并返回错误：留着一个「部分正确」
/// 的暂存目录会诱导后续步骤把半套文件装上去。这是这里唯一的破坏性动作，
/// 因此它只作用于 updates/ 之下、且作用于本次新建的版本子目录。
pub fn download(
    layout: &Layout,
    outcome: &CheckOutcome,
    progress: &mut dyn FnMut(&str),
) -> Result<Staged, UpdateError> {
    if !outcome.decision.is_update() {
        return Err(UpdateError::Refused(outcome.decision.message()));
    }
    let manifest = &outcome.manifest;
    let root = layout.updates_dir().join(manifest.version.to_string());
    // 先清掉上次失败的残留：同名版本目录必须从干净状态开始
    if root.exists() {
        std::fs::remove_dir_all(&root).map_err(|e| UpdateError::Io(format!("{root:?}: {e}")))?;
    }
    std::fs::create_dir_all(&root).map_err(|e| UpdateError::Io(format!("{root:?}: {e}")))?;

    let mut files = Vec::with_capacity(manifest.files.len());
    for entry in &manifest.files {
        progress(&format!("下载 {}（{} 字节）", entry.path, entry.size));
        let bytes = fetch::fetch(&entry.path, Some(&outcome.manifest_url))
            .map_err(|e| UpdateError::Fetch(format!("{}: {e}", entry.path)))?;
        let destination = root.join(entry.path.replace('/', std::path::MAIN_SEPARATOR_STR));
        if let Some(parent) = destination.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| UpdateError::Io(format!("{parent:?}: {e}")))?;
        }
        std::fs::write(&destination, &bytes)
            .map_err(|e| UpdateError::Io(format!("{destination:?}: {e}")))?;
        if let Err(error) = verify_file(&destination, entry) {
            let _ = std::fs::remove_dir_all(&root);
            return Err(UpdateError::Corrupt {
                path: entry.path.clone(),
                reason: error.to_string(),
            });
        }
        files.push((entry.path.clone(), destination));
    }

    // 清单本身也留一份在暂存目录里，落地时随安装一起写进去，
    // 这样 installed.json 的 source_manifest 总能对得上。
    let manifest_copy = root.join(sidelens_update::MANIFEST_FILE);
    std::fs::write(&manifest_copy, manifest.to_json())
        .map_err(|e| UpdateError::Io(format!("{manifest_copy:?}: {e}")))?;

    Ok(Staged {
        version: manifest.version,
        dir: root,
        files,
    })
}

/// 把暂存目录里的文件落到安装根，覆盖旧版本并备份。
///
/// 用 sidelens_deploy::plan + install 而不是自己写复制：与安装器共用同一套
/// 原子写/备份/清理逻辑，避免出现「安装器装得进去、更新器写不出来」。
pub fn apply_staged(
    layout: &Layout,
    outcome: &CheckOutcome,
    staged: &Staged,
    previous_version: Option<Version>,
) -> Result<String, UpdateError> {
    let manifest = &outcome.manifest;
    let plan = sidelens_deploy::plan_install(
        layout,
        &staged.dir,
        manifest,
        // installed.json 的内容由调用方拼好（它需要 registered_dshow 的真实值），
        // 这里先写一个不含注册表状态的版本，落地后由安装器/更新器回写。
        String::new(),
    )
    .map_err(UpdateError::Refused)?;

    // plan_install 会要求 installed.json 也作为写入动作；给一个占位内容就够了，
    // 真正的记录由 refresh_installed 在成功之后写。
    let report = sidelens_deploy::install::apply(&plan, layout, previous_version)
        .map_err(UpdateError::Io)?;

    // 落地后再按清单校验一遍：不信自己刚写的东西
    let failures = sidelens_update::verify_tree(layout.root(), &manifest.files);
    if !failures.is_empty() {
        let text = failures
            .iter()
            .map(|(path, error)| format!("{path}: {error}"))
            .collect::<Vec<_>>()
            .join("；");
        return Err(UpdateError::Corrupt {
            path: "安装目录".into(),
            reason: text,
        });
    }
    Ok(report.summary())
}

/// 更新完成后刷新 installed.json，保留原来的 registered_dshow 状态。
pub fn refresh_installed(layout: &Layout, version: Version, now: &str) -> Result<(), UpdateError> {
    let previous = std::fs::read_to_string(layout.installed())
        .ok()
        .and_then(|text| sidelens_update::InstalledRecord::parse(&text).ok());
    let mut record = sidelens_update::InstalledRecord::new(version, layout, now.to_string());
    if let Some(previous) = previous {
        record.registered_dshow = previous.registered_dshow;
    }
    std::fs::write(layout.installed(), record.to_json())
        .map_err(|e| UpdateError::Io(format!("{:?}: {e}", layout.installed())))
}

/// 从一份目录生成清单。
///
/// 打包脚本（tools/package-release.ps1）用它，\`verify-release.ps1\` 也用它
/// 生成第二份清单做交叉比对 —— 两份实现各自算哈希，任何一个算错都能被发现。
pub fn build_manifest(
    product_dir: &Path,
    version: &str,
    channel: &str,
    phone: sidelens_update::manifest::PhoneManifest,
    notes: &str,
    released: &str,
) -> Result<Manifest, UpdateError> {
    let version = Version::parse(version).map_err(|e| UpdateError::Refused(e.to_string()))?;
    let mut files = Vec::new();
    collect_files(product_dir, product_dir, &mut files)?;
    if files.is_empty() {
        return Err(UpdateError::Refused(format!(
            "{product_dir:?} 里没有可发布的文件"
        )));
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(Manifest {
        schema: sidelens_update::SUPPORTED_SCHEMA,
        product: sidelens_update::PRODUCT.to_string(),
        channel: channel.to_string(),
        version,
        released: released.to_string(),
        // 打包时把可升级下限设成同版本：只有一个版本的历史时，"低于它就只能重装"
        // 会让第一次发布就卡住自己。真要收紧跨大版本升级，发版时手改清单即可。
        min_upgradable_version: Version::new(0, 0, 0),
        files,
        phone,
        notes: notes.to_string(),
    })
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<ManifestFile>) -> Result<(), UpdateError> {
    let entries = std::fs::read_dir(dir).map_err(|e| UpdateError::Io(format!("{dir:?}: {e}")))?;
    for entry in entries {
        let entry = entry.map_err(|e| UpdateError::Io(e.to_string()))?;
        let path = entry.path();
        if path.is_dir() {
            collect_files(root, &path, out)?;
            continue;
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|e| UpdateError::Io(e.to_string()))?
            .to_string_lossy()
            .replace('\\', "/");
        // 打包产物里不该出现清单自己与已安装记录：它们由打包脚本分别生成。
        if relative == sidelens_update::MANIFEST_FILE || relative == sidelens_update::INSTALLED_FILE
        {
            continue;
        }
        let size = entry
            .metadata()
            .map_err(|e| UpdateError::Io(e.to_string()))?
            .len();
        let sha256 = sha256_file(&path).map_err(|e| UpdateError::Io(format!("{path:?}: {e}")))?;
        out.push(ManifestFile {
            path: relative,
            size,
            sha256,
        });
    }
    Ok(())
}

/// 把 CheckError 转成给人看的一句话（供 CLI 直接用）
pub fn describe_check(error: &CheckError) -> String {
    error.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sidelens_update::manifest::PhoneManifest;

    fn scratch(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "sidelens-update-mod-{}-{}-{name}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn phone() -> PhoneManifest {
        PhoneManifest {
            min_supported_version_code: 1,
            latest_version_code: 2,
            latest_version_name: "0.2.0".into(),
            min_supported_protocol: 1,
        }
    }

    #[test]
    fn 生成的清单能被自己解析回来() {
        let dir = scratch("build");
        std::fs::write(dir.join("sidelensd.exe"), b"bin").unwrap();
        std::fs::create_dir_all(dir.join("sdk")).unwrap();
        std::fs::write(dir.join("sdk").join("a.dll"), b"dll").unwrap();

        let manifest = build_manifest(
            &dir,
            "0.2.0",
            "stable",
            phone(),
            "note",
            "2026-11-26T00:00:00Z",
        )
        .unwrap();
        assert_eq!(manifest.files.len(), 2);
        // 路径用正斜杠
        assert!(manifest.files.iter().any(|f| f.path == "sdk/a.dll"));
        // 序列化后再解析应完全一致
        let again = Manifest::parse(&manifest.to_json()).unwrap();
        assert_eq!(manifest, again);
        // 每个文件确实能校验通过
        assert!(sidelens_update::verify_tree(&dir, &manifest.files).is_empty());
    }

    #[test]
    fn 生成清单时跳过清单与已安装记录自身() {
        let dir = scratch("skip");
        std::fs::write(dir.join("sidelensd.exe"), b"bin").unwrap();
        std::fs::write(dir.join(sidelens_update::MANIFEST_FILE), b"{}").unwrap();
        std::fs::write(dir.join(sidelens_update::INSTALLED_FILE), b"{}").unwrap();
        let manifest = build_manifest(&dir, "0.2.0", "stable", phone(), "", "").unwrap();
        assert_eq!(manifest.files.len(), 1);
        assert_eq!(manifest.files[0].path, "sidelensd.exe");
    }

    #[test]
    fn 空目录生成清单会被拒绝() {
        let dir = scratch("empty");
        assert!(build_manifest(&dir, "0.2.0", "stable", phone(), "", "").is_err());
    }

    #[test]
    fn 非法版本号被拒绝() {
        let dir = scratch("badver");
        std::fs::write(dir.join("a.exe"), b"x").unwrap();
        assert!(build_manifest(&dir, "0.2", "stable", phone(), "", "").is_err());
    }

    #[test]
    fn 检查走到本地清单并给出决策() {
        let dir = scratch("check");
        std::fs::write(dir.join("sidelensd.exe"), b"bin").unwrap();
        let manifest = build_manifest(&dir, "0.2.0", "stable", phone(), "", "").unwrap();
        std::fs::write(dir.join(sidelens_update::MANIFEST_FILE), manifest.to_json()).unwrap();

        let url = dir.join(sidelens_update::MANIFEST_FILE);
        let outcome = check(&url.display().to_string(), "0.1.0").unwrap();
        assert!(outcome.decision.is_update());

        let outcome = check(&url.display().to_string(), "0.2.0").unwrap();
        assert!(!outcome.decision.is_update());
    }

    #[test]
    fn 下载并校验后落在_updates_子目录里() {
        let dir = scratch("download");
        let product = dir.join("product");
        std::fs::create_dir_all(&product).unwrap();
        std::fs::write(product.join("sidelensd.exe"), b"binary").unwrap();
        let manifest = build_manifest(&product, "0.2.0", "stable", phone(), "", "").unwrap();
        std::fs::write(
            product.join(sidelens_update::MANIFEST_FILE),
            manifest.to_json(),
        )
        .unwrap();

        let layout = Layout::new(dir.join("install"));
        let manifest_url = product.join(sidelens_update::MANIFEST_FILE);
        let outcome = check(&manifest_url.display().to_string(), "0.1.0").unwrap();
        let mut log = Vec::new();
        let staged = download(&layout, &outcome, &mut |line| log.push(line.to_string())).unwrap();

        assert_eq!(staged.version, Version::new(0, 2, 0));
        assert!(staged.dir.ends_with("0.2.0"));
        assert_eq!(
            std::fs::read(staged.dir.join("sidelensd.exe")).unwrap(),
            b"binary"
        );
        assert!(!log.is_empty(), "应有下载进度回调");
    }

    #[test]
    fn 内容被篡改的产物在下载阶段就被拒绝() {
        let dir = scratch("tamper");
        let product = dir.join("product");
        std::fs::create_dir_all(&product).unwrap();
        std::fs::write(product.join("sidelensd.exe"), b"binary").unwrap();
        let manifest = build_manifest(&product, "0.2.0", "stable", phone(), "", "").unwrap();
        std::fs::write(
            product.join(sidelens_update::MANIFEST_FILE),
            manifest.to_json(),
        )
        .unwrap();
        // 清单写完之后再改内容 —— 模拟 CDN 上被替换过的产物
        std::fs::write(product.join("sidelensd.exe"), b"tampered").unwrap();

        let layout = Layout::new(dir.join("install"));
        let outcome = check(
            &product
                .join(sidelens_update::MANIFEST_FILE)
                .display()
                .to_string(),
            "0.1.0",
        )
        .unwrap();
        let error = download(&layout, &outcome, &mut |_| {}).unwrap_err();
        assert!(matches!(error, UpdateError::Corrupt { .. }), "{error:?}");
        // 失败后暂存目录必须被清掉
        assert!(!layout.updates_dir().join("0.2.0").exists());
    }

    #[test]
    fn 已是最新时拒绝重复下载() {
        let dir = scratch("uptodate");
        let product = dir.join("product");
        std::fs::create_dir_all(&product).unwrap();
        std::fs::write(product.join("sidelensd.exe"), b"binary").unwrap();
        let manifest = build_manifest(&product, "0.2.0", "stable", phone(), "", "").unwrap();
        std::fs::write(
            product.join(sidelens_update::MANIFEST_FILE),
            manifest.to_json(),
        )
        .unwrap();

        let layout = Layout::new(dir.join("install"));
        let outcome = check(
            &product
                .join(sidelens_update::MANIFEST_FILE)
                .display()
                .to_string(),
            "0.2.0",
        )
        .unwrap();
        let error = download(&layout, &outcome, &mut |_| {}).unwrap_err();
        assert!(matches!(error, UpdateError::Refused(_)), "{error:?}");
    }

    #[test]
    fn 落地后安装目录通过校验且记录版本更新() {
        let dir = scratch("apply");
        let product = dir.join("product");
        std::fs::create_dir_all(&product).unwrap();
        std::fs::write(product.join("sidelensd.exe"), b"v2").unwrap();
        let manifest = build_manifest(&product, "0.2.0", "stable", phone(), "", "").unwrap();
        std::fs::write(
            product.join(sidelens_update::MANIFEST_FILE),
            manifest.to_json(),
        )
        .unwrap();

        let layout = Layout::new(dir.join("install"));
        std::fs::create_dir_all(layout.root()).unwrap();
        std::fs::write(layout.exe(), b"v1").unwrap();

        let outcome = check(
            &product
                .join(sidelens_update::MANIFEST_FILE)
                .display()
                .to_string(),
            "0.1.0",
        )
        .unwrap();
        let staged = download(&layout, &outcome, &mut |_| {}).unwrap();
        let summary =
            apply_staged(&layout, &outcome, &staged, Some(Version::new(0, 1, 0))).unwrap();
        assert!(!summary.is_empty());
        assert_eq!(std::fs::read(layout.exe()).unwrap(), b"v2");
        // 备份里有旧版本
        let backup = layout
            .backup_dir(&Version::new(0, 1, 0))
            .join(sidelens_update::layout::exe_name());
        assert_eq!(std::fs::read(&backup).unwrap(), b"v1");

        refresh_installed(&layout, Version::new(0, 2, 0), "2026-11-26T00:00:00Z").unwrap();
        let record = sidelens_update::InstalledRecord::parse(
            &std::fs::read_to_string(layout.installed()).unwrap(),
        )
        .unwrap();
        assert_eq!(record.version, Version::new(0, 2, 0));
    }
}
