//! 手机端 STATE 上报里的版本字段。
//!
//! Android 端 `reportPhoneState()` 每 2 秒发一条 STATE JSON。这一层只负责
//! 把其中的版本信息取出来，**并且接受三种历史形态**：
//!
//! | 形态 | 来源 | 解析结果 |
//! | --- | --- | --- |
//! | 没有 `version` 字段 | 0.1.0 的手机端没上报 | `version_code: None` |
//! | `version` 是整数 | 简短形态，只有 versionCode | code 有值、name 空 |
//! | `version` 是对象 | 完整形态 | code/name/protocol 全有 |
//!
//! 第三种是本次新增的规范形态。前两种必须继续能读 —— 用户手上会有没升级的
//! 手机端，电脑端不能因为读不懂 STATE 就断开会话。

use serde_json::Value;

use crate::PROTOCOL_VERSION;

/// 从一条 STATE JSON 里取出的版本信息
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PhoneReport {
    /// Android `versionCode`
    pub version_code: Option<u32>,
    /// Android `versionName`
    pub version_name: String,
    /// 手机端实现的协议版本（`PhoneProtocol.VERSION`）
    pub protocol: Option<u32>,
}

impl PhoneReport {
    /// 解析 STATE 的 JSON 文本。任何结构问题都退化成"没上报"，不报错 ——
    /// 这条路径在视频流中间执行，抛错会打断共享。
    pub fn parse(text: &str) -> Self {
        let Ok(value) = serde_json::from_str::<Value>(text) else {
            return Self::default();
        };
        Self::from_value(&value)
    }

    pub fn from_value(value: &Value) -> Self {
        let mut report = Self::default();
        match value.get("version") {
            Some(Value::Number(n)) => {
                report.version_code = n.as_u64().map(|v| v as u32);
            }
            Some(Value::Object(map)) => {
                report.version_code = map.get("code").and_then(Value::as_u64).map(|v| v as u32);
                report.version_name = map
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                report.protocol = map
                    .get("protocol")
                    .and_then(Value::as_u64)
                    .map(|v| v as u32);
            }
            Some(Value::String(text)) => {
                // 有些打包链路会把版本号变成字符串；能解析就解析，不能就算没上报。
                report.version_name = text.clone();
                report.version_code = text.trim().parse::<u32>().ok();
            }
            _ => {}
        }
        // 顶层也接受 protocol，方便只关心协议兼容性的实现少传一层。
        if report.protocol.is_none() {
            report.protocol = value
                .get("protocol")
                .and_then(Value::as_u64)
                .map(|v| v as u32);
        }
        report
    }

    /// 生成规范形态的 STATE 片段，供 Android 端与测试夹具共用。
    pub fn to_json(&self) -> Value {
        let mut map = serde_json::Map::new();
        let mut version = serde_json::Map::new();
        version.insert("code".into(), Value::from(self.version_code.unwrap_or(0)));
        version.insert("name".into(), Value::from(self.version_name.clone()));
        version.insert(
            "protocol".into(),
            Value::from(self.protocol.unwrap_or(PROTOCOL_VERSION)),
        );
        map.insert("version".into(), Value::Object(version));
        Value::Object(map)
    }

    /// 一行可读摘要，用于日志
    pub fn summary(&self) -> String {
        match (self.version_code, self.protocol) {
            (Some(code), Some(protocol)) => format!(
                "{}（versionCode {code}，协议 v{protocol}）",
                if self.version_name.is_empty() {
                    "未知版本名"
                } else {
                    self.version_name.as_str()
                }
            ),
            (Some(code), None) => format!("versionCode {code}（未上报协议）"),
            (None, Some(protocol)) => format!("未上报版本（协议 v{protocol}）"),
            (None, None) => "未上报版本".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 规范形态三段全取到() {
        let text = r#"{"battery":88,"version":{"code":3,"name":"0.2.0","protocol":1}}"#;
        let report = PhoneReport::parse(text);
        assert_eq!(report.version_code, Some(3));
        assert_eq!(report.version_name, "0.2.0");
        assert_eq!(report.protocol, Some(1));
    }

    #[test]
    fn 旧版没有_version_字段时算未上报() {
        let text = r#"{"battery":88,"thermal":2,"audio":false}"#;
        let report = PhoneReport::parse(text);
        assert_eq!(report, PhoneReport::default());
    }

    #[test]
    fn 简短整数形态() {
        let report = PhoneReport::parse(r#"{"version":7}"#);
        assert_eq!(report.version_code, Some(7));
        assert_eq!(report.protocol, None);
    }

    #[test]
    fn 字符串形态() {
        let report = PhoneReport::parse(r#"{"version":"4"}"#);
        assert_eq!(report.version_code, Some(4));
        assert_eq!(report.version_name, "4");
    }

    #[test]
    fn 顶层_protocol_也认() {
        let report = PhoneReport::parse(r#"{"protocol":1}"#);
        assert_eq!(report.protocol, Some(1));
        assert_eq!(report.version_code, None);
    }

    #[test]
    fn 垃圾输入退化为未上报而不崩溃() {
        assert_eq!(PhoneReport::parse("not json"), PhoneReport::default());
        assert_eq!(PhoneReport::parse("{}"), PhoneReport::default());
        assert_eq!(PhoneReport::parse("[1,2,3]"), PhoneReport::default());
    }

    #[test]
    fn 生成与解析互逆() {
        let report = PhoneReport {
            version_code: Some(2),
            version_name: "0.2.0".into(),
            protocol: Some(1),
        };
        assert_eq!(PhoneReport::parse(&report.to_json().to_string()), report);
    }

    #[test]
    fn 摘要在三种情形下都非空() {
        for report in [
            PhoneReport::default(),
            PhoneReport {
                version_code: Some(1),
                ..Default::default()
            },
            PhoneReport {
                protocol: Some(1),
                ..Default::default()
            },
            PhoneReport {
                version_code: Some(1),
                version_name: "0.2.0".into(),
                protocol: Some(1),
            },
        ] {
            assert!(!report.summary().is_empty());
        }
    }
}
