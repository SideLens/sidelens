//! 安装布局与 `installed.json`。
//!
//! 布局契约见 `docs/release-manifest.md` §4。这里只做**纯路径计算**与
//! 序列化，不碰文件系统 —— 安装器（`sidelens-setup`）与更新器
//! （`sidelensd --update`）都要用同一份布局，而它们的 IO 权限、
//! 失败原子要求完全不同，所以 IO 留在各自那边。

use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::version::Version;

/// 产品名：注册表值名、`installed.json` 的 `product` 字段、托盘菜单文案共用
pub const PRODUCT: &str = "SideLens";
/// 安装目录名
pub const INSTALL_DIR_NAME: &str = "SideLens";
/// 卸载注册表项（`HKCU` 下的相对路径）
pub const UNINSTALL_KEY: &str = r"SoftwareMicrosoftWindowsCurrentVersionUninstallSideLens";
/// 已安装状态文件名
pub const INSTALLED_FILE: &str = "installed.json";
/// 清单文件名
pub const MANIFEST_FILE: &str = "manifest.json";
/// 更新下载目录名
pub const UPDATES_DIR: &str = "updates";
/// 旧版本备份目录名
pub const BACKUPS_DIR: &str = "backups";
/// 最多保留几份备份
pub const MAX_BACKUPS: usize = 2;

/// 安装布局。`root` 是安装根目录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    root: PathBuf,
}

impl Layout {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// `%LOCALAPPDATA%\SideLens`
    ///
    /// 用 per-user 的 `LOCALAPPDATA` 而不是 `Program Files`：安装不需要
    /// 管理员权限。代价是 DirectShow 滤镜注册（写 HKLM）装不了 ——
    /// 安装器把这种情况记进 `installed.json` 的 `registered_dshow`，
    /// 卸载时据此提示。
    pub fn default_root(local_app_data: &Path) -> PathBuf {
        local_app_data.join(INSTALL_DIR_NAME)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn exe(&self) -> PathBuf {
        self.root.join(exe_name())
    }

    pub fn manifest(&self) -> PathBuf {
        self.root.join(MANIFEST_FILE)
    }

    pub fn installed(&self) -> PathBuf {
        self.root.join(INSTALLED_FILE)
    }

    pub fn updates_dir(&self) -> PathBuf {
        self.root.join(UPDATES_DIR)
    }

    pub fn backups_dir(&self) -> PathBuf {
        self.root.join(BACKUPS_DIR)
    }

    /// 把清单里的正斜杠相对路径落到盘上
    pub fn join_relative(&self, relative: &str) -> PathBuf {
        self.root
            .join(relative.replace('/', std::path::MAIN_SEPARATOR_STR))
    }

    /// 备份目录里的子目录：`backups/<version>`
    pub fn backup_dir(&self, version: &Version) -> PathBuf {
        self.backups_dir().join(version.to_string())
    }
}

/// 可执行文件名（Windows 上带 `.exe`，其他平台仅供测试断言用）
pub fn exe_name() -> &'static str {
    if cfg!(windows) {
        "sidelensd.exe"
    } else {
        "sidelensd"
    }
}

/// 安装器自己的文件名
pub fn setup_name() -> &'static str {
    if cfg!(windows) {
        "sidelens-setup.exe"
    } else {
        "sidelens-setup"
    }
}

/// `installed.json` 的内容
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledRecord {
    pub schema: u32,
    pub product: String,
    pub version: Version,
    pub install_root: String,
    pub installed_at: String,
    /// 安装时用的清单文件名（相对安装根）
    pub source_manifest: String,
    /// 是否成功注册了 DirectShow 滤镜（需要管理员权限）
    pub registered_dshow: bool,
}

impl InstalledRecord {
    pub fn new(version: Version, layout: &Layout, installed_at: String) -> Self {
        Self {
            schema: 1,
            product: PRODUCT.to_string(),
            version,
            install_root: layout.root().display().to_string(),
            installed_at,
            source_manifest: MANIFEST_FILE.to_string(),
            registered_dshow: false,
        }
    }

    pub fn parse(text: &str) -> Result<Self, InstalledError> {
        let value: Value =
            serde_json::from_str(text).map_err(|e| InstalledError::Json(e.to_string()))?;
        let object = value
            .as_object()
            .ok_or_else(|| InstalledError::Json("顶层不是对象".into()))?;
        let raw_version = object
            .get("version")
            .and_then(Value::as_str)
            .ok_or(InstalledError::Missing("version"))?;
        let version =
            Version::parse(raw_version).map_err(|e| InstalledError::Version(e.to_string()))?;
        Ok(Self {
            schema: object.get("schema").and_then(Value::as_u64).unwrap_or(1) as u32,
            product: object
                .get("product")
                .and_then(Value::as_str)
                .unwrap_or(PRODUCT)
                .to_string(),
            version,
            install_root: object
                .get("install_root")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            installed_at: object
                .get("installed_at")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            source_manifest: object
                .get("source_manifest")
                .and_then(Value::as_str)
                .unwrap_or(MANIFEST_FILE)
                .to_string(),
            registered_dshow: object
                .get("registered_dshow")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }

    pub fn to_json(&self) -> String {
        let value = serde_json::json!({
            "schema": self.schema,
            "product": self.product,
            "version": self.version.to_string(),
            "install_root": self.install_root,
            "installed_at": self.installed_at,
            "source_manifest": self.source_manifest,
            "registered_dshow": self.registered_dshow,
        });
        serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".into())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstalledError {
    Json(String),
    Missing(&'static str),
    Version(String),
}

impl std::fmt::Display for InstalledError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            InstalledError::Json(e) => write!(f, "installed.json 无效：{e}"),
            InstalledError::Missing(field) => write!(f, "installed.json 缺少字段 {field}"),
            InstalledError::Version(e) => write!(f, "installed.json 版本无效：{e}"),
        }
    }
}

impl std::error::Error for InstalledError {}

/// 卸载注册表项里要写的值。返回 `(值名, 值)`，值统一是字符串
/// （Windows「应用和功能」把 DWORD 的 `NoModify`/`NoRepair` 也接受数字）。
///
/// 单独抽成函数是为了能在非 Windows 上断言这些文案 —— 注册表不可用，
/// 但"卸载项里写什么"可以完全离线校验。
pub fn uninstall_values(record: &InstalledRecord, setup_exe: &Path) -> Vec<(String, String)> {
    let setup = setup_exe.display().to_string();
    vec![
        ("DisplayName".into(), PRODUCT.into()),
        ("DisplayVersion".into(), record.version.to_string()),
        ("DisplayIcon".into(), record.install_root.clone()),
        ("InstallLocation".into(), record.install_root.clone()),
        ("Publisher".into(), PRODUCT.into()),
        // 用户从「应用和功能」点卸载时不带参数；静默路径带 --silent 供脚本调用。
        ("UninstallString".into(), format!("\"{setup}\" --uninstall")),
        (
            "QuietUninstallString".into(),
            format!("\"{setup}\" --uninstall --silent"),
        ),
        ("NoModify".into(), "1".into()),
        ("NoRepair".into(), "1".into()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 布局路径都挂在根目录下() {
        let layout = Layout::new(r"C:\Users\me\AppData\Local\SideLens");
        assert!(layout.exe().ends_with(exe_name()));
        assert!(layout.manifest().ends_with(MANIFEST_FILE));
        assert!(layout.installed().ends_with(INSTALLED_FILE));
        assert!(layout.updates_dir().ends_with(UPDATES_DIR));
        assert!(layout.backups_dir().ends_with(BACKUPS_DIR));
    }

    #[test]
    fn 默认根目录在_local_appdata_下() {
        let root = Layout::default_root(Path::new(r"D:\local"));
        assert_eq!(root, Path::new(r"D:\local").join("SideLens"));
    }

    #[test]
    fn 相对路径按平台分隔符拼接() {
        let layout = Layout::new("root");
        let joined = layout.join_relative("sdk/sidelens_dshow.dll");
        assert!(joined.ends_with("sidelens_dshow.dll"));
        assert!(joined.starts_with("root"));
    }

    #[test]
    fn 备份目录按版本号命名() {
        let layout = Layout::new("root");
        let dir = layout.backup_dir(&Version::new(0, 1, 0));
        assert!(dir.ends_with("0.1.0"));
    }

    #[test]
    fn 已安装记录往返() {
        let layout = Layout::new(r"C:\x\SideLens");
        let record = InstalledRecord::new(
            Version::new(0, 2, 0),
            &layout,
            "2026-11-26T00:00:00Z".into(),
        );
        let again = InstalledRecord::parse(&record.to_json()).expect("应能解析");
        assert_eq!(record, again);
        assert_eq!(again.source_manifest, MANIFEST_FILE);
        assert!(!again.registered_dshow);
    }

    #[test]
    fn 已安装记录缺版本时报错() {
        assert_eq!(
            InstalledRecord::parse(r#"{"product":"SideLens"}"#),
            Err(InstalledError::Missing("version"))
        );
        assert!(matches!(
            InstalledRecord::parse(r#"{"version":"oops"}"#),
            Err(InstalledError::Version(_))
        ));
    }

    #[test]
    fn 卸载项包含静默与交互两条命令() {
        let layout = Layout::new(r"C:\x\SideLens");
        let record = InstalledRecord::new(Version::new(0, 2, 0), &layout, String::new());
        let values = uninstall_values(&record, Path::new(r"C:\x\SideLens\sidelens-setup.exe"));
        let map: std::collections::HashMap<_, _> = values.into_iter().collect();
        assert_eq!(map["DisplayName"], "SideLens");
        assert_eq!(map["DisplayVersion"], "0.2.0");
        assert!(map["UninstallString"].contains("--uninstall"));
        assert!(map["QuietUninstallString"].contains("--silent"));
        assert_eq!(map["NoModify"], "1");
        assert_eq!(map["NoRepair"], "1");
        // 带空格的路径必须被引号包住，否则「应用和功能」拆参数会把路径腰斩
        assert!(map["UninstallString"].starts_with('"'));
    }

    #[test]
    fn 卸载项的值名不重复() {
        let layout = Layout::new("root");
        let record = InstalledRecord::new(Version::new(0, 1, 0), &layout, String::new());
        let values = uninstall_values(&record, Path::new("setup.exe"));
        let mut names: Vec<_> = values.iter().map(|(k, _)| k.clone()).collect();
        names.sort();
        let count = names.len();
        names.dedup();
        assert_eq!(names.len(), count, "值名有重复");
    }
}
