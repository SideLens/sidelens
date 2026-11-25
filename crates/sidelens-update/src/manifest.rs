//! `manifest.json` 的读写与校验。
//!
//! 契约见 `docs/release-manifest.md`。这一层只做「文件 → 结构体」和
//! 「结构体 → 文件」，不做任何 IO 顺序决策 —— 谁先写谁后写由安装器和
//! 更新器决定，它们对失败原子的要求不一样。

use std::collections::BTreeSet;

use serde_json::{Map, Value};

use crate::version::{Version, VersionError};

/// 本程序能读懂的最高清单 schema。
pub const SUPPORTED_SCHEMA: u32 = 1;

/// 一个待安装文件
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestFile {
    /// 相对安装根的路径，正斜杠分隔。已规范化，不含 `..`、不以 `/` 开头。
    pub path: String,
    pub size: u64,
    /// 小写十六进制 sha256（64 字符）
    pub sha256: String,
}

/// 手机端升级信息
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhoneManifest {
    pub min_supported_version_code: u32,
    pub latest_version_code: u32,
    pub latest_version_name: String,
    pub min_supported_protocol: u32,
}

impl Default for PhoneManifest {
    fn default() -> Self {
        // 保守默认：不要求升级、协议下限取当前协议版本。
        // 清单里缺 `phone` 时用这一份，行为等价于「不提示、不放行更老的协议」。
        Self {
            min_supported_version_code: 0,
            latest_version_code: 0,
            latest_version_name: String::new(),
            min_supported_protocol: crate::PROTOCOL_VERSION,
        }
    }
}

/// 解析后的发布清单
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub schema: u32,
    pub product: String,
    pub channel: String,
    pub version: Version,
    pub released: String,
    pub min_upgradable_version: Version,
    pub files: Vec<ManifestFile>,
    pub phone: PhoneManifest,
    pub notes: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManifestError {
    /// JSON 语法或类型不对
    Json(String),
    /// 缺必填字段
    Missing(&'static str),
    /// `files` 里的路径不合格
    BadPath { path: String, reason: &'static str },
    /// `files` 里同一路径出现两次
    DuplicatePath(String),
    /// sha256 不是 64 位小写十六进制
    BadHash { path: String, value: String },
    /// 版本号不可解析
    Version {
        field: &'static str,
        error: VersionError,
    },
    /// `files` 为空
    NoFiles,
}

impl std::fmt::Display for ManifestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ManifestError::Json(e) => write!(f, "清单 JSON 无效：{e}"),
            ManifestError::Missing(field) => write!(f, "清单缺少字段 {field}"),
            ManifestError::BadPath { path, reason } => {
                write!(f, "清单里的路径 \"{path}\" 不合格：{reason}")
            }
            ManifestError::DuplicatePath(path) => write!(f, "清单里路径重复：{path}"),
            ManifestError::BadHash { path, value } => {
                write!(f, "\"{path}\" 的 sha256 \"{value}\" 不是 64 位小写十六进制")
            }
            ManifestError::Version { field, error } => {
                write!(f, "清单字段 {field} 的版本无效：{error}")
            }
            ManifestError::NoFiles => write!(f, "清单 files 为空"),
        }
    }
}

impl std::error::Error for ManifestError {}

impl Manifest {
    /// 从 JSON 文本解析。
    ///
    /// 未知字段被忽略（契约要求字段只增不减），但已知字段做**严格**校验：
    /// 宁可安装器报「清单不合格」也不要写出一半文件再失败。
    pub fn parse(text: &str) -> Result<Self, ManifestError> {
        let value: Value =
            serde_json::from_str(text).map_err(|e| ManifestError::Json(e.to_string()))?;
        let object = value
            .as_object()
            .ok_or_else(|| ManifestError::Json("顶层不是对象".into()))?;

        let schema = object
            .get("schema")
            .and_then(Value::as_u64)
            .ok_or(ManifestError::Missing("schema"))? as u32;
        let product = string_field(object, "product")?;
        let channel = object
            .get("channel")
            .and_then(Value::as_str)
            .unwrap_or("stable")
            .to_string();
        let version = version_field(object, "version")?;
        let released = object
            .get("released")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        // 缺 min_upgradable_version 时按「所有版本都能升」处理：
        // 老清单没有这个字段，卡住用户比放开更糟。
        let min_upgradable_version = match object.get("min_upgradable_version") {
            None | Some(Value::Null) => Version::new(0, 0, 0),
            Some(_) => version_field(object, "min_upgradable_version")?,
        };
        let files = parse_files(object)?;
        let phone = parse_phone(object)?;
        let notes = object
            .get("notes")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();

        Ok(Self {
            schema,
            product,
            channel,
            version,
            released,
            min_upgradable_version,
            files,
            phone,
            notes,
        })
    }

    /// 序列化成带缩进的 JSON（供打包脚本与测试夹具使用）。
    pub fn to_json(&self) -> String {
        let files: Vec<Value> = self
            .files
            .iter()
            .map(|f| {
                serde_json::json!({
                    "path": f.path,
                    "size": f.size,
                    "sha256": f.sha256,
                })
            })
            .collect();
        let value = serde_json::json!({
            "schema": self.schema,
            "product": self.product,
            "channel": self.channel,
            "version": self.version.to_string(),
            "released": self.released,
            "min_upgradable_version": self.min_upgradable_version.to_string(),
            "files": files,
            "phone": {
                "min_supported_version_code": self.phone.min_supported_version_code,
                "latest_version_code": self.phone.latest_version_code,
                "latest_version_name": self.phone.latest_version_name,
                "min_supported_protocol": self.phone.min_supported_protocol,
            },
            "notes": self.notes,
        });
        serde_json::to_string_pretty(&value).unwrap_or_else(|_| "{}".into())
    }

    /// 按路径找条目
    pub fn file(&self, path: &str) -> Option<&ManifestFile> {
        self.files.iter().find(|f| f.path == path)
    }
}

fn string_field(object: &Map<String, Value>, field: &'static str) -> Result<String, ManifestError> {
    object
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty())
        .ok_or(ManifestError::Missing(field))
}

fn version_field(
    object: &Map<String, Value>,
    field: &'static str,
) -> Result<Version, ManifestError> {
    let raw = object
        .get(field)
        .and_then(Value::as_str)
        .ok_or(ManifestError::Missing(field))?;
    Version::parse(raw).map_err(|error| ManifestError::Version { field, error })
}

fn parse_files(object: &Map<String, Value>) -> Result<Vec<ManifestFile>, ManifestError> {
    let raw = object
        .get("files")
        .and_then(Value::as_array)
        .ok_or(ManifestError::Missing("files"))?;
    if raw.is_empty() {
        return Err(ManifestError::NoFiles);
    }
    let mut seen = BTreeSet::new();
    let mut files = Vec::with_capacity(raw.len());
    for item in raw {
        let path = item
            .get("path")
            .and_then(Value::as_str)
            .ok_or(ManifestError::Missing("files[].path"))?;
        normalize_relative(path)?;
        if !seen.insert(path.to_string()) {
            return Err(ManifestError::DuplicatePath(path.to_string()));
        }
        let size = item
            .get("size")
            .and_then(Value::as_u64)
            .ok_or(ManifestError::Missing("files[].size"))?;
        let sha256 = item
            .get("sha256")
            .and_then(Value::as_str)
            .ok_or(ManifestError::Missing("files[].sha256"))?;
        if !is_sha256(sha256) {
            return Err(ManifestError::BadHash {
                path: path.to_string(),
                value: sha256.to_string(),
            });
        }
        files.push(ManifestFile {
            path: path.to_string(),
            size,
            sha256: sha256.to_string(),
        });
    }
    Ok(files)
}

fn parse_phone(object: &Map<String, Value>) -> Result<PhoneManifest, ManifestError> {
    let Some(phone) = object.get("phone").and_then(Value::as_object) else {
        return Ok(PhoneManifest::default());
    };
    let mut manifest = PhoneManifest::default();
    if let Some(v) = phone
        .get("min_supported_version_code")
        .and_then(Value::as_u64)
    {
        manifest.min_supported_version_code = v as u32;
    }
    if let Some(v) = phone.get("latest_version_code").and_then(Value::as_u64) {
        manifest.latest_version_code = v as u32;
    }
    if let Some(v) = phone.get("latest_version_name").and_then(Value::as_str) {
        manifest.latest_version_name = v.to_string();
    }
    if let Some(v) = phone.get("min_supported_protocol").and_then(Value::as_u64) {
        manifest.min_supported_protocol = v as u32;
    }
    Ok(manifest)
}

/// 校验并规范化一个清单内的相对路径。
///
/// 拒绝的理由都是**安全**的：清单不能把文件写到安装根之外。
/// 这是整个安装/更新链路里唯一一处「输入可以决定写哪里」的地方，
/// 因此判据要窄到可以逐条枚举：
///
/// - 空路径、绝对路径（`C:`、`\\server\`、`/etc`）
/// - 任何含 `..` 的段
/// - 反斜杠（契约规定正斜杠；出现反斜杠说明打包脚本拼错了，
///   而在 Windows 上 `a\\..\\b` 又确实能穿越，所以直接拒）
/// - 盘符冒号
/// - 结尾斜杠（指向目录而不是文件）
/// - NUL 字符
pub fn normalize_relative(path: &str) -> Result<(), ManifestError> {
    let bad = |reason: &'static str| ManifestError::BadPath {
        path: path.to_string(),
        reason,
    };
    if path.is_empty() {
        return Err(bad("空路径"));
    }
    if path.contains('\0') {
        return Err(bad("含 NUL"));
    }
    if path.contains('\\') {
        return Err(bad("含反斜杠，契约要求正斜杠"));
    }
    if path.contains(':') {
        return Err(bad("含盘符冒号"));
    }
    if path.starts_with('/') {
        return Err(bad("是绝对路径"));
    }
    if path.ends_with('/') {
        return Err(bad("以斜杠结尾，指向目录"));
    }
    for segment in path.split('/') {
        if segment.is_empty() {
            return Err(bad("含空路径段"));
        }
        if segment == "." || segment == ".." {
            return Err(bad("含 . 或 .. 段"));
        }
    }
    Ok(())
}

/// 是否是 64 位小写十六进制
pub fn is_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_json() -> String {
        serde_json::json!({
            "schema": 1,
            "product": "SideLens",
            "channel": "stable",
            "version": "0.2.0",
            "released": "2026-11-26T08:00:00Z",
            "min_upgradable_version": "0.1.0",
            "files": [
                { "path": "sidelensd.exe", "size": 3182080, "sha256": "a".repeat(64) },
                { "path": "sdk/sidelens_dshow.dll", "size": 302080, "sha256": "b".repeat(64) }
            ],
            "phone": {
                "min_supported_version_code": 1,
                "latest_version_code": 3,
                "latest_version_name": "0.2.0",
                "min_supported_protocol": 1
            },
            "notes": "首个带安装包的版本"
        })
        .to_string()
    }

    #[test]
    fn 解析完整清单() {
        let m = Manifest::parse(&sample_json()).expect("应能解析");
        assert_eq!(m.version, Version::new(0, 2, 0));
        assert_eq!(m.min_upgradable_version, Version::new(0, 1, 0));
        assert_eq!(m.files.len(), 2);
        assert_eq!(m.files[1].path, "sdk/sidelens_dshow.dll");
        assert_eq!(m.phone.latest_version_code, 3);
    }

    #[test]
    fn 序列化再解析是幂等的() {
        let m = Manifest::parse(&sample_json()).expect("应能解析");
        let again = Manifest::parse(&m.to_json()).expect("序列化结果应能再解析");
        assert_eq!(m, again);
    }

    #[test]
    fn 缺少_phone_段时用保守默认值() {
        let mut value: Value = serde_json::from_str(&sample_json()).unwrap();
        value.as_object_mut().unwrap().remove("phone");
        let m = Manifest::parse(&value.to_string()).unwrap();
        assert_eq!(m.phone.latest_version_code, 0);
        assert_eq!(m.phone.min_supported_protocol, crate::PROTOCOL_VERSION);
    }

    #[test]
    fn 缺少_min_upgradable_version_时视为零() {
        let mut value: Value = serde_json::from_str(&sample_json()).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .remove("min_upgradable_version");
        let m = Manifest::parse(&value.to_string()).unwrap();
        assert_eq!(m.min_upgradable_version, Version::new(0, 0, 0));
    }

    #[test]
    fn 路径穿越与绝对路径一律拒绝() {
        for path in [
            "../evil.exe",
            "a/../../evil.exe",
            "C:/Windows/evil.exe",
            "/etc/passwd",
            "sdk\\evil.dll",
            "dir/",
            "./x.exe",
            "a//b.exe",
        ] {
            assert!(normalize_relative(path).is_err(), "{path} 不该通过路径校验");
        }
        assert!(normalize_relative("sidelensd.exe").is_ok());
        assert!(normalize_relative("sdk/sidelens_dshow.dll").is_ok());
    }

    #[test]
    fn sha256_只接受六十四位小写十六进制() {
        assert!(is_sha256(&"0".repeat(64)));
        assert!(!is_sha256(&"0".repeat(63)));
        assert!(!is_sha256(&"A".repeat(64)), "大写不该通过");
        assert!(!is_sha256(&"g".repeat(64)));
    }

    #[test]
    fn 重复路径被拒绝() {
        let value = serde_json::json!({
            "schema": 1,
            "product": "SideLens",
            "version": "0.2.0",
            "files": [
                { "path": "a.exe", "size": 1, "sha256": "a".repeat(64) },
                { "path": "a.exe", "size": 1, "sha256": "b".repeat(64) }
            ]
        });
        assert_eq!(
            Manifest::parse(&value.to_string()),
            Err(ManifestError::DuplicatePath("a.exe".into()))
        );
    }

    #[test]
    fn 空文件列表被拒绝() {
        let value = serde_json::json!({
            "schema": 1,
            "product": "SideLens",
            "version": "0.2.0",
            "files": []
        });
        assert_eq!(
            Manifest::parse(&value.to_string()),
            Err(ManifestError::NoFiles)
        );
    }

    #[test]
    fn 未知字段被忽略而不是报错() {
        let mut value: Value = serde_json::from_str(&sample_json()).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("future_field".into(), Value::Bool(true));
        assert!(Manifest::parse(&value.to_string()).is_ok());
    }

    #[test]
    fn 版本字段非法时指出是哪个字段() {
        let mut value: Value = serde_json::from_str(&sample_json()).unwrap();
        value
            .as_object_mut()
            .unwrap()
            .insert("version".into(), Value::String("0.2".into()));
        match Manifest::parse(&value.to_string()) {
            Err(ManifestError::Version { field, .. }) => assert_eq!(field, "version"),
            other => panic!("得到 {other:?}"),
        }
    }
}
