//! SideLens 的发布层纯逻辑：版本、清单、哈希、布局、升级决策。
//!
//! # 这个 crate 的边界
//!
//! **有 IO 的一律不在这里。** 网络下载在 `sidelensd` 的更新模块里，
//! 注册表与文件替换在 `sidelens-setup` 里，Android 侧的提示在 Kotlin 里。
//! 这里只有输入 → 输出的纯函数，因此可以在任何平台上跑全部测试 ——
//! 包括 Linux CI（`tools/verify-cross-platform.ps1` 会需要它）。
//!
//! 三处消费方共用同一份判据，是这一层存在的全部理由：
//!
//! | 消费方 | 用它回答 |
//! | --- | --- |
//! | `sidelens-setup` | 装的是哪个版本、卸载项写什么 |
//! | `sidelensd --update` | 该不该更新、下载下来对不对、手机端要不要提示 |
//! | GUI / 托盘 | 当前版本、有一版新的 |
//!
//! 契约文档：`docs/release-manifest.md`。

pub mod decision;
pub mod hash;
pub mod layout;
pub mod manifest;
pub mod phone;
pub mod version;

pub use decision::{decide, phone_hint, Decision, PhoneHint};
pub use hash::{sha256_bytes, sha256_file, verify_file, verify_tree, CheckError};
pub use layout::{
    uninstall_values, InstalledRecord, Layout, BACKUPS_DIR, INSTALLED_FILE, INSTALL_DIR_NAME,
    MANIFEST_FILE, MAX_BACKUPS, PRODUCT, UNINSTALL_KEY, UPDATES_DIR,
};
pub use manifest::{
    is_sha256, normalize_relative, Manifest, ManifestError, ManifestFile, PhoneManifest,
    SUPPORTED_SCHEMA,
};
pub use phone::PhoneReport;
pub use version::{Version, VersionError};

/// 手机链路协议版本。与 `crates/sidelens-phone-proto` 的 `VERSION` 必须一致。
///
/// # 为什么这里还要再写一遍
///
/// `sidelens-update` 不能依赖 `sidelens-phone-proto`：后者带 `std::io` 的
/// 读写逻辑，且是设备链路的一层，把发布决策绑上去会让 Linux CI 上的
/// 发布层测试也必须编整个协议栈。两处常量的一致性由测试守住 ——
/// 见 `crates/sidelens-phone-proto/tests/protocol_version.rs`。
pub const PROTOCOL_VERSION: u32 = 1;

/// 本 crate 的语义版本，与 workspace 的 `version` 一致
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    #[test]
    fn 协议版本常量不是占位值() {
        // 真正的跨 crate 断言在 crates/sidelens-phone-proto/tests/protocol_version.rs。
        // 这里只保证本地常量不是占位的 0；用运行期比较避免 clippy 的
        // assertions_on_constants 把它当成恒定断言优化掉。
        let version = super::PROTOCOL_VERSION;
        assert_ne!(version, 0);
    }

    #[test]
    fn 语义版本可被本层的解析器接受() {
        assert!(
            super::Version::parse(super::VERSION).is_ok(),
            "{}",
            super::VERSION
        );
    }
}
