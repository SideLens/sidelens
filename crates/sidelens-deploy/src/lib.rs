//! SideLens 的落盘层：动作计划、原子写、备份、注册表。
//!
//! 契约见 `docs/release-manifest.md` §4。这一层被两个调用方共用：
//!
//! | 调用方 | 用的部分 |
//! | --- | --- |
//! | `sidelens-setup` | 全部 |
//! | `sidelensd --update` / `--check-update` | `plan` + `install`（不碰注册表，注册表由安装器负责） |
//!
//! 分层：
//!
//! | 模块 | 平台 | 做什么 |
//! | --- | --- | --- |
//! | `plan` | 跨平台 | 纯数据：要复制什么、要删什么、注册表写什么 |
//! | `install` | 跨平台 | 执行计划：原子写、备份、保留最近 N 份 |
//! | `registry` | Windows | 卸载项 + `DllRegisterServer` |
//!
//! 原子写的三条不变量（每个都由 `install` 的测试钉住）：
//!
//! 1. 每个文件独立原子 —— 先写 `.tmp`、`sync_all`、再改名，中途崩不会留半个 exe。
//! 2. 覆盖前先备份 —— 升级把旧文件放进 `backups/<旧版本>`，最多留 `MAX_BACKUPS` 份。
//! 3. 失败要能说出来 —— 错误信息必须带路径与原因，CLI 据此给非 0 退出码。

pub mod install;
pub mod plan;
pub mod registry;

pub use install::{apply, apply_uninstall, copy_atomic, prune_backups, ApplyReport};
pub use plan::{
    dshow_dll_in, plan_install, plan_uninstall, FileAction, InstallPlan, RegistryAction,
    UninstallPlan,
};
pub use registry::{
    is_elevated, register_dshow, remove_uninstall, unregister_dshow, write_uninstall,
};

/// 供 `--dry-run` 打印的计划渲染。两个调用方输出必须一致 ——
/// 用户看到的「会动哪些文件」不该因为用安装器还是用更新器而不同。
pub fn render_plan(plan: &InstallPlan) -> String {
    let mut out = String::new();
    for action in &plan.files {
        match action {
            FileAction::Copy { from, to } => {
                out.push_str(&format!("  复制 {} → {}\n", from.display(), to.display()))
            }
            FileAction::Write { to, .. } => out.push_str(&format!("  写入 {}\n", to.display())),
            FileAction::RemoveDir { path } => out.push_str(&format!("  删除 {}\n", path.display())),
        }
    }
    if let Some(dll) = plan.register_dshow.as_ref() {
        out.push_str(&format!("  注册 DirectShow 滤镜 {}\n", dll.display()));
    }
    out
}

/// 卸载计划的渲染
pub fn render_uninstall_plan(plan: &UninstallPlan) -> String {
    let mut out = String::new();
    for action in &plan.files {
        if let FileAction::RemoveDir { path } = action {
            out.push_str(&format!("  删除 {}\n", path.display()));
        }
    }
    if let Some(dll) = plan.unregister_dshow.as_ref() {
        out.push_str(&format!("  反注册 DirectShow 滤镜 {}\n", dll.display()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use sidelens_update::manifest::{Manifest, ManifestFile, PhoneManifest};
    use sidelens_update::{Layout, Version};
    use std::path::PathBuf;

    fn manifest(files: &[&str]) -> Manifest {
        Manifest {
            schema: 1,
            product: "SideLens".into(),
            channel: "stable".into(),
            version: Version::new(0, 2, 0),
            released: String::new(),
            min_upgradable_version: Version::new(0, 0, 0),
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

    #[test]
    fn 计划渲染列出每一个动作() {
        let source = std::env::temp_dir().join(format!("deploy-render-{}", std::process::id()));
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("sidelensd.exe"), b"x").unwrap();
        std::fs::write(source.join("sidelens_dshow.dll"), b"y").unwrap();
        let layout = Layout::new(PathBuf::from(r"C:\inst\SideLens"));
        let plan = plan_install(
            &layout,
            &source,
            &manifest(&["sidelensd.exe", "sidelens_dshow.dll"]),
            "{}".into(),
        )
        .unwrap();
        let text = render_plan(&plan);
        assert!(text.contains("sidelensd.exe"));
        assert!(text.contains("manifest.json"));
        assert!(text.contains("DirectShow"));
        let _ = std::fs::remove_dir_all(&source);
    }

    #[test]
    fn 卸载计划渲染不列复制动作() {
        let layout = Layout::new(PathBuf::from(r"C:\Users\me\SideLens"));
        let plan = plan_uninstall(&layout, None).unwrap();
        let text = render_uninstall_plan(&plan);
        assert!(text.contains("删除"));
        assert!(!text.contains("复制"));
    }
}
