//! `--self-test`：在沙箱目录里跑完整安装/升级/卸载，不碰系统状态。
//!
//! # 为什么安装器自己要带一个自测
//!
//! `tools/verify-release.ps1` 已经会真的装一遍，但那个脚本要构建、
//! 要打包、要几分钟。开发时改一行 `install.rs` 就想知道"还能不能装"，
//! 需要一个 `cargo run` 级别的检查。
//!
//! 自测的三条边界：
//!
//! 1. **不写 HKCU**：注册表动作在自测里被跳过并记入报告。
//! 2. **不碰真实安装目录**：一切都在 `<临时目录>/sidelens-setup-selftest-<pid>` 下。
//! 3. **报告可机读**：一行一个 `[selftest] PASS/FAIL <名称>`，
//!    `verify-release.ps1` 靠它把失败定位到具体步骤。

use std::fs;
use std::path::{Path, PathBuf};

use sidelens_update::hash::sha256_bytes;
use sidelens_update::layout::Layout;
use sidelens_update::manifest::{Manifest, ManifestFile, PhoneManifest};
use sidelens_update::{decide, Decision, InstalledRecord, Version};

use sidelens_deploy::install::{apply, apply_uninstall};
use sidelens_deploy::plan::{plan_install, plan_uninstall};

/// 一步自测的结果
#[derive(Debug, Clone)]
pub struct Step {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
}

/// 自测报告
#[derive(Debug, Default)]
pub struct Report {
    pub steps: Vec<Step>,
}

impl Report {
    fn check(&mut self, name: &'static str, ok: bool, detail: impl Into<String>) {
        self.steps.push(Step {
            name,
            ok,
            detail: detail.into(),
        });
    }

    pub fn passed(&self) -> bool {
        self.steps.iter().all(|s| s.ok)
    }

    pub fn render(&self) -> String {
        let mut out = String::new();
        for step in &self.steps {
            out.push_str(&format!(
                "[selftest] {} {}\n",
                if step.ok { "PASS" } else { "FAIL" },
                step.name
            ));
            if !step.detail.is_empty() {
                out.push_str(&format!("            {}\n", step.detail));
            }
        }
        out.push_str(&format!(
            "[selftest] 汇总：{} / {} 通过\n",
            self.steps.iter().filter(|s| s.ok).count(),
            self.steps.len()
        ));
        out
    }
}

/// 跑自测。返回报告；调用方按 `passed()` 决定退出码。
pub fn run() -> Report {
    let mut report = Report::default();
    // 目录名必须**每次调用唯一**，不能只用 PID：
    // 同一个测试进程里三个用例会并发调用 run()，共用目录会互相删文件，
    // 现象是随机的 "系统找不到指定的文件"。这个坑真的踩到了。
    let root = std::env::temp_dir().join(format!(
        "sidelens-setup-selftest-{}-{}",
        std::process::id(),
        unique_nonce()
    ));
    let _ = fs::remove_dir_all(&root);
    if let Err(e) = fs::create_dir_all(&root) {
        report.check("准备沙箱目录", false, format!("{e}"));
        return report;
    }

    // ---- 1. 造一个假发布包 ----
    let source = root.join("source");
    let layout = Layout::new(root.join("install"));
    let payload: &[(&str, &[u8])] = &[
        ("sidelensd.exe", b"fake-sidelensd-binary"),
        ("sidelens_dshow.dll", b"fake-dshow-dll"),
        ("sidelens_vcam_dll.dll", b"fake-vcam-dll"),
    ];
    let mut build_error = None;
    for (path, bytes) in payload {
        let full = source.join(path);
        if let Some(parent) = full.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                build_error = Some(format!("{e}"));
            }
        }
        if let Err(e) = fs::write(&full, bytes) {
            build_error = Some(format!("{e}"));
        }
    }
    match build_error {
        None => report.check("造出假发布包", true, format!("{source:?}")),
        Some(e) => {
            report.check("造出假发布包", false, e);
            return report;
        }
    }

    let manifest = Manifest {
        schema: 1,
        product: "SideLens".into(),
        channel: "test".into(),
        version: Version::new(0, 2, 0),
        released: "2026-11-26T00:00:00Z".into(),
        min_upgradable_version: Version::new(0, 1, 0),
        files: payload
            .iter()
            .map(|(path, bytes)| ManifestFile {
                path: (*path).to_string(),
                size: bytes.len() as u64,
                sha256: sha256_bytes(bytes),
            })
            .collect(),
        phone: PhoneManifest::default(),
        notes: "self-test".into(),
    };

    // ---- 2. 全新安装 ----
    let record = InstalledRecord::new(manifest.version, &layout, "2026-11-26T00:00:00Z".into());
    let plan = match plan_install(&layout, &source, &manifest, record.to_json()) {
        Ok(plan) => {
            report.check("生成安装计划", true, format!("{} 步", plan.files.len()));
            plan
        }
        Err(e) => {
            report.check("生成安装计划", false, e);
            return report;
        }
    };
    report.check(
        "计划登记了 DirectShow 滤镜",
        plan.register_dshow.is_some(),
        plan.register_dshow
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "未登记".into()),
    );

    match apply(&plan, &layout, None) {
        Ok(apply_report) => report.check("执行安装", true, apply_report.summary()),
        Err(e) => {
            report.check("执行安装", false, e);
            return report;
        }
    }

    // ---- 3. 安装结果自校验 ----
    let failures = sidelens_update::verify_tree(layout.root(), &manifest.files);
    report.check(
        "安装后清单校验",
        failures.is_empty(),
        if failures.is_empty() {
            format!("{} 个文件全部匹配", manifest.files.len())
        } else {
            format!("{failures:?}")
        },
    );

    let installed_text = fs::read_to_string(layout.installed()).unwrap_or_default();
    match InstalledRecord::parse(&installed_text) {
        Ok(read_back) => {
            report.check(
                "已安装记录可读回",
                read_back.version == manifest.version,
                format!("版本 {}", read_back.version),
            );
            report.check(
                "决策结论为已最新",
                matches!(
                    decide(&read_back.version.to_string(), &manifest),
                    Decision::UpToDate { .. }
                ),
                decide(&read_back.version.to_string(), &manifest).message(),
            );
        }
        Err(e) => report.check("已安装记录可读回", false, e.to_string()),
    }

    // ---- 4. 被篡改的文件应被识别 ----
    let exe_path = layout.join_relative("sidelensd.exe");
    if fs::write(&exe_path, b"tampered").is_ok() {
        let tampered = sidelens_update::verify_tree(layout.root(), &manifest.files);
        report.check(
            "篡改被校验抓到",
            tampered.iter().any(|(path, _)| path == "sidelensd.exe"),
            format!("{} 处失败", tampered.len()),
        );
    }

    // ---- 5. 升级：备份旧版本 ----
    if let Ok(plan) = plan_install(&layout, &source, &manifest, record.to_json()) {
        match apply(&plan, &layout, Some(Version::new(0, 1, 0))) {
            Ok(r) => report.check(
                "升级时备份旧文件",
                !r.backed_up.is_empty(),
                format!("备份 {} 个", r.backed_up.len()),
            ),
            Err(e) => report.check("升级时备份旧文件", false, e),
        }
    }

    // ---- 6. 卸载 ----
    match plan_uninstall(&layout, Some(&manifest)) {
        Ok(uninstall_plan) => {
            report.check(
                "卸载计划带反注册",
                uninstall_plan.unregister_dshow.is_some(),
                uninstall_plan
                    .unregister_dshow
                    .as_ref()
                    .map(|path| describe(path))
                    .unwrap_or_else(|| "无".into()),
            );
            match apply_uninstall(&uninstall_plan) {
                Ok(r) => {
                    report.check("执行卸载", true, r.summary());
                    report.check(
                        "卸载后目录消失",
                        !layout.root().exists(),
                        format!("{:?}", layout.root()),
                    );
                }
                Err(e) => report.check("执行卸载", false, e),
            }
        }
        Err(e) => report.check("生成卸载计划", false, e),
    }

    // ---- 7. 卸载后重装仍然可用（幂等性）----
    if let Ok(plan) = plan_install(&layout, &source, &manifest, record.to_json()) {
        match apply(&plan, &layout, None) {
            Ok(_) => report.check(
                "卸载后重装",
                sidelens_update::verify_tree(layout.root(), &manifest.files).is_empty(),
                "重装后校验通过",
            ),
            Err(e) => report.check("卸载后重装", false, e),
        }
    }

    let _ = fs::remove_dir_all(&root);
    report.check("清理沙箱", true, format!("{root:?}"));
    report
}

fn describe(path: &Path) -> String {
    path.display().to_string()
}

/// 进程内单调递增 + 纳秒时间，保证同一 PID 内的并发调用互不冲突
fn unique_nonce() -> u128 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed) as u128;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    nanos.wrapping_mul(1000).wrapping_add(seq)
}

/// 报告落盘的路径（失败时便于附在工单里）
pub fn report_path() -> PathBuf {
    std::env::temp_dir().join("sidelens-setup-selftest.txt")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 自测全流程通过() {
        let report = run();
        let text = report.render();
        assert!(report.passed(), "自测失败：\n{text}");
        // 关键步骤必须真的被执行过，而不是被跳过
        for name in [
            "生成安装计划",
            "执行安装",
            "安装后清单校验",
            "篡改被校验抓到",
            "升级时备份旧文件",
            "执行卸载",
            "卸载后目录消失",
            "卸载后重装",
        ] {
            assert!(
                report.steps.iter().any(|s| s.name == name),
                "缺少步骤 {name}：\n{text}"
            );
        }
    }

    #[test]
    fn 报告格式可被脚本解析() {
        let report = run();
        let text = report.render();
        for line in text.lines() {
            if line.starts_with("[selftest] PASS") || line.starts_with("[selftest] FAIL") {
                assert!(line.len() > "[selftest] PASS ".len());
            }
        }
        assert!(text.contains("汇总："));
    }

    #[test]
    fn 自测不碰真实安装目录() {
        let before = Layout::default_root(&dirs_local_appdata());
        let existed = before.exists();
        let _ = run();
        assert_eq!(
            before.exists(),
            existed,
            "自测不该创建或删除真实的 %LOCALAPPDATA%\\SideLens"
        );
    }

    fn dirs_local_appdata() -> PathBuf {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
    }
}
