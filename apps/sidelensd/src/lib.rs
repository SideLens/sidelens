//! sidelensd 的可测部分。
//!
//! 目前只放**更新链路**：它由纯逻辑（版本比较、清单校验）加一层网络
//! （HTTP 取回）组成，两者都不依赖 Windows、不依赖真实摄像头、不需要
//! 用户坐在屏幕前 —— 正好是可以在 CI 里真跑的那一块。
//!
//! 采集、解码、GUI、虚拟摄像头仍在二进制里（见 src/main.rs 的模块声明）：
//! 它们要么是 Windows 专属，要么必须在真机上验证，放进 lib 会让每个集成
//! 测试都链接一遍整套媒体栈。

pub mod update;
pub mod update_cli;

/// 重新导出发布层，方便外部（测试、脚本、GUI）只用一套版本判断
pub use sidelens_update as release;
