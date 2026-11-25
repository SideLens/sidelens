//! 手机端升级提示的下发策略。
//!
//! 电脑端每 2 秒收到一条手机 STATE。这条 STATE 里带 versionCode / versionName /
//! protocol（见 crates/sidelens-update/src/phone.rs 的三种历史形态）。本模块
//! 负责回答两件事：
//!
//! 1. 该不该给这台手机发提示（判据在 sidelens_update::phone_hint）；
//! 2. **要不要现在发** —— 每 2 秒重发同一条 CONTROL 是纯粹的浪费：
//!    手机端会反复弹同样的横幅，日志会被刷满。
//!
//! 因此这里维护一个「上一次发过的提示」，只在**提示变化**时下发。
//! "变化"包括：从无到有、级别提升、目标版本变了、以及从有到无（需要显式
//! 撤销一次，否则手机端会把旧提示一直挂着）。
//!
//! 把这块逻辑放在 lib 里而不是 phone_bridge 里，是因为它是纯函数式的状态机，
//! 可以在没有手机、没有网络的条件下把每个分支都跑一遍。

use sidelens_update::decision::PhoneHint;
use sidelens_update::manifest::Manifest;
use sidelens_update::phone::PhoneReport;
use sidelens_update::{phone_hint, Version};

/// 上一次下发过的提示（用于去重）
#[derive(Debug, Default)]
pub struct HintTracker {
    last: Option<PhoneHint>,
    sent: u64,
}

impl HintTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// 已下发的次数
    pub fn sent_count(&self) -> u64 {
        self.sent
    }

    /// 上一次下发的提示（None 表示"当前无提示"）
    pub fn last(&self) -> Option<&PhoneHint> {
        self.last.as_ref()
    }

    /// 观察一条 STATE，返回**需要发给手机的 CONTROL JSON**（不需要发时返回 None）。
    ///
    /// # 为什么会返回「撤销」消息
    ///
    /// 场景：用户先装了老手机端，电脑端提示"必须升级"；然后手机升级了，
    /// 新 STATE 报上来 versionCode 已经够新。此时如果什么都不发，
    /// 手机端界面上那条"请升级"的横幅会一直挂着 —— 因为它是被控制消息
    /// 写上去了，没有任何东西会去清它。
    ///
    /// 所以"从有到无"必须显式发一条 level=none 的消息。
    pub fn observe(&mut self, state_json: &str, manifest: Option<&Manifest>) -> Option<String> {
        // 没有清单就没有判据。**不发任何东西**：宁可不提示，
        // 也不要按一个猜的下限去骚扰所有用户。
        let manifest = manifest?;
        let report = PhoneReport::parse(state_json);
        let hint = phone_hint(&report, manifest);
        self.observe_hint(&hint, &report)
    }

    /// 直接喂一个已算好的 hint（测试与 GUI 手动触发用）
    pub fn observe_hint(&mut self, hint: &PhoneHint, report: &PhoneReport) -> Option<String> {
        if let Some(previous) = self.last.as_ref() {
            if previous == hint {
                return None;
            }
        } else if matches!(hint, PhoneHint::None) {
            // 从来没有提示过，现在也没有 —— 不必发"撤销"
            return None;
        }

        let payload = match hint {
            PhoneHint::None => {
                self.last = None;
                self.sent += 1;
                return Some(
                    serde_json::json!({ "phone_upgrade": { "level": "none" } }).to_string(),
                );
            }
            PhoneHint::Unknown => return None,
            PhoneHint::Recommended {
                reported,
                latest,
                latest_name,
            } => serde_json::json!({
                "phone_upgrade": {
                    "level": "recommended",
                    "reported_code": reported,
                    "latest_code": latest,
                    "latest_name": latest_name,
                    "message": hint.message(),
                }
            }),
            PhoneHint::MustUpgrade {
                reported,
                min_supported,
                latest,
                latest_name,
            } => serde_json::json!({
                "phone_upgrade": {
                    "level": "must",
                    "reported_code": reported,
                    "min_code": min_supported,
                    "latest_code": latest,
                    "latest_name": latest_name,
                    "message": hint.message(),
                }
            }),
            PhoneHint::Unsupported {
                protocol,
                min_protocol,
                latest,
                latest_name,
            } => serde_json::json!({
                "phone_upgrade": {
                    "level": "unsupported",
                    "reported_protocol": protocol,
                    "min_protocol": min_protocol,
                    "latest_code": latest,
                    "latest_name": latest_name,
                    "message": hint.message(),
                }
            }),
        };

        self.last = Some(hint.clone());
        self.sent += 1;
        // 把手机上报的版本也带上：手机端日志里能直接对上号
        let mut payload = payload;
        if let Some(code) = report.version_code {
            if let Some(inner) = payload
                .get_mut("phone_upgrade")
                .and_then(|value| value.as_object_mut())
            {
                inner.insert("reported_code".into(), code.into());
            }
        }
        Some(payload.to_string())
    }

    /// 会话结束时清空跟踪状态
    pub fn reset(&mut self) {
        self.last = None;
    }
}

/// 从安装目录或环境变量里取清单，供升级提示使用。
///
/// 优先级：
/// 1. SIDELENS_MANIFEST 环境变量（测试与内网部署）
/// 2. 安装根下的 manifest.json
///
/// 都没有时返回 None —— 此时不下发任何提示，行为与"没有这个功能"一致。
pub fn load_manifest(layout_root: &std::path::Path) -> Option<Manifest> {
    if let Some(path) = std::env::var_os("SIDELENS_MANIFEST") {
        let text = std::fs::read_to_string(path).ok()?;
        return Manifest::parse(&text).ok();
    }
    let text = std::fs::read_to_string(layout_root.join(sidelens_update::MANIFEST_FILE)).ok()?;
    Manifest::parse(&text).ok()
}

/// 给 GUI 状态栏用的一句话
pub fn describe(manifest: Option<&Manifest>, current: Option<&Version>) -> String {
    match (manifest, current) {
        (Some(manifest), Some(current)) => format!(
            "当前安装 {}；清单 {}（手机端最新 versionCode {}）",
            current, manifest.version, manifest.phone.latest_version_code
        ),
        (Some(manifest), None) => format!("发布清单 {}，本机未登记安装记录", manifest.version),
        (None, Some(current)) => format!("当前安装 {current}；没有可用的发布清单"),
        (None, None) => "没有安装记录，也没有发布清单".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sidelens_update::manifest::PhoneManifest;

    fn manifest(min: u32, latest: u32, min_protocol: u32) -> Manifest {
        Manifest {
            schema: 1,
            product: "SideLens".into(),
            channel: "stable".into(),
            version: Version::new(0, 2, 0),
            released: String::new(),
            min_upgradable_version: Version::new(0, 0, 0),
            files: vec![sidelens_update::manifest::ManifestFile {
                path: "sidelensd.exe".into(),
                size: 1,
                sha256: "0".repeat(64),
            }],
            phone: PhoneManifest {
                min_supported_version_code: min,
                latest_version_code: latest,
                latest_version_name: "0.2.0".into(),
                min_supported_protocol: min_protocol,
            },
            notes: String::new(),
        }
    }

    fn state(code: u32, protocol: u32) -> String {
        serde_json::json!({ "version": { "code": code, "name": "0.1.0", "protocol": protocol } })
            .to_string()
    }

    #[test]
    fn 版本够新时什么都不发() {
        let mut tracker = HintTracker::new();
        let manifest = manifest(1, 2, 1);
        assert!(tracker.observe(&state(2, 1), Some(&manifest)).is_none());
        assert_eq!(tracker.sent_count(), 0);
    }

    #[test]
    fn 必须升级时发一次且只发一次() {
        let mut tracker = HintTracker::new();
        let manifest = manifest(2, 3, 1);
        let first = tracker
            .observe(&state(1, 1), Some(&manifest))
            .expect("应下发");
        let value: serde_json::Value = serde_json::from_str(&first).unwrap();
        assert_eq!(value["phone_upgrade"]["level"], "must");
        assert_eq!(value["phone_upgrade"]["min_code"], 2);
        assert_eq!(value["phone_upgrade"]["reported_code"], 1);
        assert!(value["phone_upgrade"]["message"]
            .as_str()
            .unwrap()
            .contains("升级"));

        // 同样的状态再来一次：不该重复下发（手机每 2 秒都发 STATE）
        assert!(tracker.observe(&state(1, 1), Some(&manifest)).is_none());
        assert_eq!(tracker.sent_count(), 1);
    }

    #[test]
    fn 手机升级之后显式撤销提示() {
        let mut tracker = HintTracker::new();
        let manifest = manifest(2, 3, 1);
        assert!(tracker.observe(&state(1, 1), Some(&manifest)).is_some());
        // 手机升级到 versionCode 3
        let revoke = tracker
            .observe(&state(3, 1), Some(&manifest))
            .expect("应撤销");
        let value: serde_json::Value = serde_json::from_str(&revoke).unwrap();
        assert_eq!(value["phone_upgrade"]["level"], "none");
        // 撤销之后继续上报同样状态，不该再发
        assert!(tracker.observe(&state(3, 1), Some(&manifest)).is_none());
        assert_eq!(tracker.sent_count(), 2);
    }

    #[test]
    fn 建议升级与必须升级之间会重新下发() {
        let mut tracker = HintTracker::new();
        let recommended = manifest(1, 3, 1);
        assert!(tracker.observe(&state(2, 1), Some(&recommended)).is_some());
        // 电脑端配置收紧：最低版本提高到 3
        let must = manifest(3, 3, 1);
        let second = tracker.observe(&state(2, 1), Some(&must)).expect("应收紧");
        let value: serde_json::Value = serde_json::from_str(&second).unwrap();
        assert_eq!(value["phone_upgrade"]["level"], "must");
        assert_eq!(tracker.sent_count(), 2);
    }

    #[test]
    fn 协议过老优先于版本判断() {
        let mut tracker = HintTracker::new();
        let manifest = manifest(1, 3, 2);
        let payload = tracker
            .observe(&state(99, 1), Some(&manifest))
            .expect("应下发");
        let value: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(value["phone_upgrade"]["level"], "unsupported");
        assert_eq!(value["phone_upgrade"]["min_protocol"], 2);
        assert_eq!(value["phone_upgrade"]["reported_protocol"], 1);
    }

    #[test]
    fn 没有清单时一个字节都不发() {
        let mut tracker = HintTracker::new();
        assert!(tracker.observe(&state(1, 1), None).is_none());
        assert_eq!(tracker.sent_count(), 0);
    }

    #[test]
    fn 手机没上报版本时不发未知提示() {
        let mut tracker = HintTracker::new();
        let manifest = manifest(1, 3, 1);
        // 老手机端没有 version 字段
        assert!(tracker
            .observe(r#"{"battery":80}"#, Some(&manifest))
            .is_none());
        assert_eq!(tracker.sent_count(), 0);
    }

    #[test]
    fn 垃圾_state_不会让跟踪器崩() {
        let mut tracker = HintTracker::new();
        let manifest = manifest(1, 3, 1);
        for payload in ["", "not json", "[]", "{}"] {
            assert!(tracker.observe(payload, Some(&manifest)).is_none());
        }
    }

    #[test]
    fn 重置之后同一个提示会重新下发() {
        let mut tracker = HintTracker::new();
        let manifest = manifest(2, 3, 1);
        assert!(tracker.observe(&state(1, 1), Some(&manifest)).is_some());
        assert!(tracker.observe(&state(1, 1), Some(&manifest)).is_none());
        tracker.reset();
        // 新会话开始，需要重新告诉这台手机
        assert!(tracker.observe(&state(1, 1), Some(&manifest)).is_some());
    }

    #[test]
    fn 描述文案在四种组合下都非空() {
        let manifest = manifest(1, 2, 1);
        let current = Version::new(0, 1, 0);
        for text in [
            describe(Some(&manifest), Some(&current)),
            describe(Some(&manifest), None),
            describe(None, Some(&current)),
            describe(None, None),
        ] {
            assert!(!text.trim().is_empty());
        }
    }
}
