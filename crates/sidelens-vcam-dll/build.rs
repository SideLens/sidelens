//! 构建脚本
//!
//! # 为什么这里不链接 .def
//!
//! `cargo test` 会为 cdylib 生成测试 harness 可执行文件。
//! 如果此时链接了 .def（它声明 `LIBRARY xxx.dll`），
//! 产物会变成 DLL 格式，测试二进制无法执行，报
//! "不是有效的 Win32 应用程序 (os error 193)"。
//!
//! build script 里**无法可靠区分** test 与正常构建
//! （`CARGO_CFG_TEST` 在 build script 环境不可用）。
//!
//! 所以 .def 的链接交给 `cargo xtask dll`：
//! 只有真正要产出可分发 DLL 时才加 `/DEF:` 参数。
//!
//! 不加 .def 的代价只是链接器警告 LNK4104（导出应为 PRIVATE），
//! 不影响功能 —— 导出表仍然正确。

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
}
