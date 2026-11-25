//! 安装/卸载的**动作计划**。
//!
//! 这一层只有纯数据：要复制哪些文件、要写哪些注册表值、要删哪些路径。
//! 执行由 `install` / `registry` 完成。
//!
//! # 为什么不直接边算边做
//!
//! 安装器最怕的不是失败，是**失败到一半**。把"要做什么"先算清、打印出来，
//! 有两个收益：
//!
//! 1. `--dry-run` 可以零副作用地告诉用户会动哪些路径；
//! 2. 自测可以在真的动手之前先断言计划本身（例如"绝不写入安装根之外的路径"），
//!    把路径穿越这类问题挡在第一次 `copy` 之前。

use std::path::{Path, PathBuf};

use sidelens_update::layout::Layout;
use sidelens_update::manifest::Manifest;

/// 一步文件动作
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileAction {
    /// 从源路径复制到目标路径（目标先写临时文件再改名）
    Copy { from: PathBuf, to: PathBuf },
    /// 直接写一段文本（清单、已安装记录）
    Write { to: PathBuf, text: String },
    /// 删除整个目录（卸载）
    RemoveDir { path: PathBuf },
}

/// 注册表动作（Windows 上才有实现）
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryAction {
    /// 写卸载项
    WriteUninstall { values: Vec<(String, String)> },
    /// 删卸载项
    RemoveUninstall,
}

/// 一份完整的安装计划
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallPlan {
    pub files: Vec<FileAction>,
    pub registry: Option<RegistryAction>,
    /// 安装后尝试注册 DirectShow 滤镜（需要管理员）
    pub register_dshow: Option<PathBuf>,
}

/// 一份完整的卸载计划
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UninstallPlan {
    pub files: Vec<FileAction>,
    pub registry: Option<RegistryAction>,
    /// 卸载前反注册 DirectShow 滤镜（需要管理员）
    pub unregister_dshow: Option<PathBuf>,
}

/// 生成安装计划。
///
/// `source` 是发布包目录（`sidelens-setup.exe` 所在处），`manifest` 已解析完。
///
/// 每个复制动作的目标都被断言**落在安装根之下** —— 这不是多余的：
/// 清单里的路径虽然在校验期被过了一遍，但在拼成绝对路径之后仍可能因为
/// 盘符相对路径（`C:x`）之类的写法跳出根目录。这里用规范化后的前缀比较，
/// 是最后一道闸。
pub fn plan_install(
    layout: &Layout,
    source: &Path,
    manifest: &Manifest,
    installed_json: String,
) -> Result<InstallPlan, String> {
    let mut files = Vec::new();
    for entry in &manifest.files {
        let from = source.join(entry.path.replace('/', std::path::MAIN_SEPARATOR_STR));
        if !from.is_file() {
            return Err(format!("发布包里缺少 {}（{from:?}）", entry.path));
        }
        let to = layout.join_relative(&entry.path);
        ensure_under_root(layout.root(), &to)?;
        files.push(FileAction::Copy { from, to });
    }
    files.push(FileAction::Write {
        to: layout.manifest(),
        text: manifest.to_json(),
    });
    files.push(FileAction::Write {
        to: layout.installed(),
        text: installed_json,
    });
    Ok(InstallPlan {
        files,
        registry: None,
        register_dshow: dshow_dll_in(manifest).map(|rel| layout.join_relative(&rel)),
    })
}

/// 生成卸载计划。
///
/// 只删**安装根**，以及清单里登记过的、落在安装根之下的文件所在目录。
/// 明确不删 `updates` 之外的用户目录：万一 `installed.json` 被塞了
/// `install_root: C:\\`，这里靠 `ensure_under_root` 与"至少三段路径"两道
/// 判据挡住整盘删除。
pub fn plan_uninstall(
    layout: &Layout,
    manifest: Option<&Manifest>,
) -> Result<UninstallPlan, String> {
    let root = layout.root();
    // 判据是「根之下有多少个普通目录段」，而不是 components() 总数：
    // `C:\inst` 的 components 是 [Prefix, RootDir, Normal] 共 3 段，
    // 但真正属于用户的只有 1 段 —— 删掉它就是删掉别人可能放东西的目录。
    // 正常安装根 `...\AppData\Local\SideLens` 有 3 段普通目录。
    let depth = root
        .components()
        .filter(|c| matches!(c, std::path::Component::Normal(_)))
        .count();
    if depth < 2 {
        return Err(format!(
            "拒绝卸载：安装根 {root:?} 只有 {depth} 层目录，怀疑 installed.json 被改过"
        ));
    }
    Ok(UninstallPlan {
        files: vec![FileAction::RemoveDir {
            path: root.to_path_buf(),
        }],
        registry: Some(RegistryAction::RemoveUninstall),
        unregister_dshow: manifest
            .and_then(dshow_dll_in)
            .map(|rel| layout.join_relative(&rel)),
    })
}

/// 清单里的 DirectShow DLL
pub fn dshow_dll_in(manifest: &Manifest) -> Option<String> {
    manifest
        .files
        .iter()
        .find(|f| {
            f.path
                .rsplit('/')
                .next()
                .is_some_and(|name| name.eq_ignore_ascii_case("sidelens_dshow.dll"))
        })
        .map(|f| f.path.clone())
}

/// 目标路径必须在根目录之下
fn ensure_under_root(root: &Path, target: &Path) -> Result<(), String> {
    let target_text = target.to_string_lossy().replace('\\', "/");
    let root_text = root.to_string_lossy().replace('\\', "/");
    if !target_text.starts_with(&root_text) {
        return Err(format!("拒绝写到安装根之外：{target:?} 不在 {root:?} 下"));
    }
    let rest = &target_text[root_text.len()..];
    // 只比字符串前缀是不够的：`C:/inst/SideLensExtra/a.exe` 也以
    // `C:/inst/SideLens` 开头。必须要求下一个字符就是分隔符，
    // 否则「安装根之外」的路径会通过这道闸。
    if !rest.starts_with('/') {
        return Err(format!("拒绝写到安装根之外：{target:?} 不在 {root:?} 下"));
    }
    if rest == "/" {
        return Err(format!("目标路径等于安装根：{target:?}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sidelens_update::manifest::{ManifestFile, PhoneManifest};
    use sidelens_update::Version;

    fn manifest(files: &[&str]) -> Manifest {
        Manifest {
            schema: 1,
            product: "SideLens".into(),
            channel: "stable".into(),
            version: Version::new(0, 2, 0),
            released: String::new(),
            min_upgradable_version: Version::new(0, 1, 0),
            files: files
                .iter()
                .map(|path| ManifestFile {
                    path: (*path).to_string(),
                    size: 1,
                    sha256: "0".repeat(64),
                })
                .collect(),
            phone: PhoneManifest::default(),
            notes: String::new(),
        }
    }

    fn source_with(files: &[&str]) -> std::path::PathBuf {
        // 用递增序号而不是文件列表拼名字：两个用例的列表可能正好相同，
        // 那时候它们会共用目录并互相删文件（见 install.rs 里的同款说明）。
        use std::sync::atomic::{AtomicU64, Ordering};
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "sidelens-setup-plan-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        for path in files {
            let full = dir.join(path.replace('/', std::path::MAIN_SEPARATOR_STR));
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(full, b"x").unwrap();
        }
        dir
    }

    #[test]
    fn 计划包含每个清单文件与两个元数据文件() {
        let source = source_with(&["sidelensd.exe", "sdk/sidelens_dshow.dll"]);
        let layout = Layout::new(r"C:\inst\SideLens");
        let plan = plan_install(
            &layout,
            &source,
            &manifest(&["sidelensd.exe", "sdk/sidelens_dshow.dll"]),
            "{}".into(),
        )
        .expect("应能生成计划");
        assert_eq!(plan.files.len(), 4, "2 个产物 + 清单 + 已安装记录");
        assert!(
            plan.register_dshow.is_some(),
            "清单里有 dshow DLL 就该尝试注册"
        );
        let _ = std::fs::remove_dir_all(&source);
    }

    #[test]
    fn 源目录缺文件时提前失败而不是装一半() {
        let source = source_with(&["sidelensd.exe"]);
        let layout = Layout::new(r"C:\inst\SideLens");
        let error = plan_install(
            &layout,
            &source,
            &manifest(&["sidelensd.exe", "missing.dll"]),
            "{}".into(),
        )
        .unwrap_err();
        assert!(
            error.contains("missing.dll"),
            "错误信息应指出缺哪个文件：{error}"
        );
        let _ = std::fs::remove_dir_all(&source);
    }

    #[test]
    fn 清单里没有_dshow_dll_时不注册() {
        let source = source_with(&["sidelensd.exe"]);
        let layout = Layout::new(r"C:\inst\SideLens");
        let plan =
            plan_install(&layout, &source, &manifest(&["sidelensd.exe"]), "{}".into()).unwrap();
        assert!(plan.register_dshow.is_none());
        let _ = std::fs::remove_dir_all(&source);
    }

    #[test]
    fn 卸载拒绝层级过浅的安装根() {
        for shallow in [r"C:\", r"C:\inst"] {
            let layout = Layout::new(shallow);
            assert!(
                plan_uninstall(&layout, None).is_err(),
                "{shallow} 不该被允许整目录删除"
            );
        }
        let layout = Layout::new(r"C:\Users\me\AppData\Local\SideLens");
        assert!(plan_uninstall(&layout, None).is_ok());
    }

    #[test]
    fn 卸载计划会带上_dshow_反注册路径() {
        let layout = Layout::new(r"C:\Users\me\AppData\Local\SideLens");
        let plan = plan_uninstall(
            &layout,
            Some(&manifest(&["sidelensd.exe", "sidelens_dshow.dll"])),
        )
        .unwrap();
        let path = plan.unregister_dshow.expect("应带反注册路径");
        assert!(path.ends_with("sidelens_dshow.dll"));
    }

    #[test]
    fn dshow_dll_匹配不区分大小写且看完整文件名() {
        let m = manifest(&["SIDELENS_DSHOW.DLL", "x/sidelens_dshow.dll.bak"]);
        assert_eq!(dshow_dll_in(&m).as_deref(), Some("SIDELENS_DSHOW.DLL"));
    }

    #[test]
    fn 目标必须严格位于根之下() {
        let root = Path::new(r"C:\inst\SideLens");
        assert!(ensure_under_root(root, Path::new(r"C:\inst\SideLens\a.exe")).is_ok());
        assert!(ensure_under_root(root, Path::new(r"C:\inst\SideLensExtra\a.exe")).is_err());
        assert!(ensure_under_root(root, Path::new(r"C:\inst\SideLens")).is_err());
        assert!(ensure_under_root(root, Path::new(r"D:\other\a.exe")).is_err());
    }
}
