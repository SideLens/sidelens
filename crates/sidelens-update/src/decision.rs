//! 升级决策。
//!
//! 契约见 `docs/release-manifest.md` §5。这一层**没有** IO、没有时间、没有
//! 随机 —— 同样的两个输入永远得到同一个结果，因此每条边界都能在单测里钉死。
//!
//! # 为什么决策与动作分开
//!
//! 「该不该更新」和「怎么更新」的失败模式完全不同：前者错一次就是用户的
//! 界面显示错误提示，后者错一次会写出半个安装目录。把它们放在一个函数里
//! 会导致后者的问题必须通过文件系统才能测出来。这里只做前者。

use crate::manifest::Manifest;
use crate::phone::PhoneReport;
use crate::version::Version;

/// 桌面端升级决策
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// 已是最新（或本地版本更高）
    UpToDate { current: Version, latest: Version },
    /// 有可用更新
    UpdateAvailable {
        current: Version,
        latest: Version,
        channel: String,
    },
    /// 本地版本低于清单声明的可原地升级下限
    ReinstallRequired {
        current: Version,
        min_upgradable: Version,
        latest: Version,
    },
    /// 清单来自比本程序更新的版本
    UnsupportedManifest { schema: u32, supported: u32 },
    /// 本地记录的版本号不可解析
    InvalidVersion { raw: String, reason: String },
}

impl Decision {
    /// 是否应该真的去下载
    pub fn is_update(&self) -> bool {
        matches!(self, Decision::UpdateAvailable { .. })
    }

    /// 给状态栏/日志的一行中文
    pub fn message(&self) -> String {
        match self {
            Decision::UpToDate { current, .. } => format!("已是最新版本 {current}"),
            Decision::UpdateAvailable {
                current,
                latest,
                channel,
            } => format!("发现新版本 {latest}（当前 {current}，通道 {channel}）"),
            Decision::ReinstallRequired {
                current,
                min_upgradable,
                latest,
            } => format!("当前 {current} 低于可升级下限 {min_upgradable}，请先卸载再安装 {latest}"),
            Decision::UnsupportedManifest { schema, supported } => {
                format!("发布清单版本 {schema} 高于本程序支持的 {supported}，请升级 SideLens")
            }
            Decision::InvalidVersion { raw, reason } => {
                format!("本地版本号 \"{raw}\" 无法解析：{reason}")
            }
        }
    }
}

/// 比较本地版本与清单，给出决策。
///
/// 判据顺序**不可交换**：
///
/// 1. 清单 schema 超前 → 直接 `UnsupportedManifest`。
///    这一条必须最先判：更新的清单可能用新的版本语义，此时任何版本比较结果
///    都不可信，硬比会得出「该升级」然后装出一个本程序读不懂的布局。
/// 2. 本地版本号解析失败 → `InvalidVersion`。
///    `installed.json` 是可能被手改的（它就在安装目录里），不能被它拖崩。
/// 3. 低于可升级下限 → `ReinstallRequired`。
/// 4. 最新 ≤ 当前 → `UpToDate`。用 `<=` 而不是 `==`：
///    从 0.3.0 回退分发 0.2.0 时，不该把 0.3.0 的用户降级。
pub fn decide(current: &str, manifest: &Manifest) -> Decision {
    if manifest.schema > crate::manifest::SUPPORTED_SCHEMA {
        return Decision::UnsupportedManifest {
            schema: manifest.schema,
            supported: crate::manifest::SUPPORTED_SCHEMA,
        };
    }
    let current = match Version::parse(current) {
        Ok(v) => v,
        Err(error) => {
            return Decision::InvalidVersion {
                raw: current.to_string(),
                reason: error.to_string(),
            }
        }
    };
    if current < manifest.min_upgradable_version {
        return Decision::ReinstallRequired {
            current,
            min_upgradable: manifest.min_upgradable_version,
            latest: manifest.version,
        };
    }
    if manifest.version <= current {
        return Decision::UpToDate {
            current,
            latest: manifest.version,
        };
    }
    Decision::UpdateAvailable {
        current,
        latest: manifest.version,
        channel: manifest.channel.clone(),
    }
}

/// 手机端升级提示
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhoneHint {
    /// 手机端版本在支持范围内
    None,
    /// 手机端还没上报版本（老版本 App 没有这个字段）
    Unknown,
    /// 有新版本但可以继续用
    Recommended {
        reported: u32,
        latest: u32,
        latest_name: String,
    },
    /// 低于支持下限，必须升级才能继续共享
    MustUpgrade {
        reported: u32,
        min_supported: u32,
        latest: u32,
        latest_name: String,
    },
    /// 协议版本太老，电脑端无法与它建立会话
    Unsupported {
        protocol: u32,
        min_protocol: u32,
        latest: u32,
        latest_name: String,
    },
}

impl PhoneHint {
    pub fn needs_action(&self) -> bool {
        matches!(
            self,
            PhoneHint::MustUpgrade { .. } | PhoneHint::Unsupported { .. }
        )
    }

    pub fn message(&self) -> String {
        match self {
            PhoneHint::None => String::new(),
            PhoneHint::Unknown => "手机端未上报版本，无法确认是否需要升级".into(),
            PhoneHint::Recommended {
                reported,
                latest,
                latest_name,
            } => format!("手机端有新版 {latest_name}（{reported} → {latest}），建议更新"),
            PhoneHint::MustUpgrade {
                reported,
                min_supported,
                latest_name,
                ..
            } => format!("手机端版本过旧（{reported} < {min_supported}），请升级到 {latest_name}"),
            PhoneHint::Unsupported {
                protocol,
                min_protocol,
                latest_name,
                ..
            } => format!(
                "手机端协议 v{protocol} 低于电脑端要求的 v{min_protocol}，请升级到 {latest_name}"
            ),
        }
    }
}

/// 根据手机端上报的 STATE 与清单里的 phone 段给出提示。
///
/// 协议下限的优先级最高：协议不兼容时「版本够不够新」已经没有意义，
/// 会话本身建不起来。
pub fn phone_hint(report: &PhoneReport, manifest: &Manifest) -> PhoneHint {
    let phone = &manifest.phone;
    let latest_name = if phone.latest_version_name.is_empty() {
        format!("versionCode {}", phone.latest_version_code)
    } else {
        phone.latest_version_name.clone()
    };
    if let Some(protocol) = report.protocol {
        if protocol < phone.min_supported_protocol {
            return PhoneHint::Unsupported {
                protocol,
                min_protocol: phone.min_supported_protocol,
                latest: phone.latest_version_code,
                latest_name,
            };
        }
    }
    let Some(code) = report.version_code else {
        return PhoneHint::Unknown;
    };
    if code < phone.min_supported_version_code {
        return PhoneHint::MustUpgrade {
            reported: code,
            min_supported: phone.min_supported_version_code,
            latest: phone.latest_version_code,
            latest_name,
        };
    }
    if code < phone.latest_version_code {
        return PhoneHint::Recommended {
            reported: code,
            latest: phone.latest_version_code,
            latest_name,
        };
    }
    PhoneHint::None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::PhoneManifest;

    fn manifest(version: &str, min_upgradable: &str) -> Manifest {
        Manifest {
            schema: 1,
            product: "SideLens".into(),
            channel: "stable".into(),
            version: Version::parse(version).unwrap(),
            released: String::new(),
            min_upgradable_version: Version::parse(min_upgradable).unwrap(),
            files: vec![crate::manifest::ManifestFile {
                path: "sidelensd.exe".into(),
                size: 1,
                sha256: "0".repeat(64),
            }],
            phone: PhoneManifest {
                min_supported_version_code: 1,
                latest_version_code: 3,
                latest_version_name: "0.2.0".into(),
                min_supported_protocol: 1,
            },
            notes: String::new(),
        }
    }

    #[test]
    fn 主线升级() {
        let d = decide("0.1.0", &manifest("0.2.0", "0.1.0"));
        assert!(d.is_update());
        assert_eq!(
            d,
            Decision::UpdateAvailable {
                current: Version::new(0, 1, 0),
                latest: Version::new(0, 2, 0),
                channel: "stable".into(),
            }
        );
    }

    #[test]
    fn 同版本与更高本地版本都不降级() {
        let m = manifest("0.2.0", "0.1.0");
        assert_eq!(
            decide("0.2.0", &m),
            Decision::UpToDate {
                current: Version::new(0, 2, 0),
                latest: Version::new(0, 2, 0)
            }
        );
        // 回滚分发：本地 0.3.0、清单 0.2.0
        let d = decide("0.3.0", &m);
        assert!(!d.is_update(), "不能把 0.3.0 降级到 0.2.0");
    }

    #[test]
    fn 低于可升级下限要求重装() {
        assert_eq!(
            decide("0.0.9", &manifest("0.2.0", "0.1.0")),
            Decision::ReinstallRequired {
                current: Version::new(0, 0, 9),
                min_upgradable: Version::new(0, 1, 0),
                latest: Version::new(0, 2, 0),
            }
        );
    }

    #[test]
    fn 清单_schema_超前时优先报不支持() {
        let mut m = manifest("0.2.0", "0.1.0");
        m.schema = crate::manifest::SUPPORTED_SCHEMA + 1;
        assert_eq!(
            decide("0.1.0", &m),
            Decision::UnsupportedManifest {
                schema: m.schema,
                supported: crate::manifest::SUPPORTED_SCHEMA
            }
        );
    }

    #[test]
    fn 本地版本号被手改成垃圾时给可读错误() {
        match decide("not-a-version", &manifest("0.2.0", "0.1.0")) {
            Decision::InvalidVersion { raw, .. } => assert_eq!(raw, "not-a-version"),
            other => panic!("得到 {other:?}"),
        }
    }

    #[test]
    fn 决策消息都非空() {
        for d in [
            decide("0.1.0", &manifest("0.2.0", "0.1.0")),
            decide("0.2.0", &manifest("0.2.0", "0.1.0")),
            decide("0.0.1", &manifest("0.2.0", "0.1.0")),
            decide("x", &manifest("0.2.0", "0.1.0")),
        ] {
            assert!(!d.message().trim().is_empty(), "{d:?} 消息为空");
        }
    }

    #[test]
    fn 手机端版本提示的四条分支() {
        let m = manifest("0.2.0", "0.1.0");
        let report = |code: Option<u32>, protocol: Option<u32>| PhoneReport {
            version_code: code,
            version_name: String::new(),
            protocol,
        };
        assert_eq!(phone_hint(&report(None, Some(1)), &m), PhoneHint::Unknown);
        assert!(matches!(
            phone_hint(&report(Some(0), Some(1)), &m),
            PhoneHint::MustUpgrade { .. }
        ));
        assert!(matches!(
            phone_hint(&report(Some(2), Some(1)), &m),
            PhoneHint::Recommended { .. }
        ));
        assert_eq!(phone_hint(&report(Some(3), Some(1)), &m), PhoneHint::None);
        assert_eq!(phone_hint(&report(Some(9), Some(1)), &m), PhoneHint::None);
    }

    #[test]
    fn 协议过老优先于版本判断() {
        let m = manifest("0.2.0", "0.1.0");
        let report = PhoneReport {
            version_code: Some(99),
            version_name: "9.9".into(),
            protocol: Some(0),
        };
        let hint = phone_hint(&report, &m);
        assert!(matches!(hint, PhoneHint::Unsupported { protocol: 0, .. }));
        assert!(hint.needs_action());
        assert!(hint.message().contains("协议"));
    }

    #[test]
    fn 只有必须升级才算需要动作() {
        let recommended = PhoneHint::Recommended {
            reported: 2,
            latest: 3,
            latest_name: "0.2.0".into(),
        };
        assert!(!recommended.needs_action());
    }
}
