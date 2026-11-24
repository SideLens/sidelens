//! 给 example / test 嵌一个 comctl32 v6 的 manifest
//!
//! # 为什么必须有这一步
//!
//! 没有 manifest 的进程，`InitCommonControlsEx` 加载的是 **comctl32 v5**，
//! 按钮、复选框、下拉框会退回 Win95 的经典外观（方块灰、没有圆角、
//! 标题栏是深蓝渐变）。这不是「丑一点」的问题 —— 用户要的是「极致的原生
//! UI 体验」，而经典外观在 200% 缩放下连字都会糊。
//!
//! # 为什么用链接器参数而不是 `rc.exe`
//!
//! 常规做法是写一个 `.rc` 用 `rc.exe` 编译成 `.res` 再链进去，但那要求
//! 构建机上有 Windows SDK 的 `rc.exe`（cargo 不会帮你找路径）。
//! MSVC 的 `link.exe` 自带 `/MANIFESTINPUT:` 参数，可以直接吃一个 XML
//! 文件并合并进 PE 的 RT_MANIFEST 资源 —— 零外部工具、零第三方 crate。
//!
//! # 为什么只作用于 example / test
//!
//! 库本身没有链接阶段，`cargo:rustc-link-arg` 对它无效也无害；真正需要
//! 视觉样式的是**可执行文件**。将来 `apps/` 下的真实客户端要嵌同一份
//! manifest，把这里的常量复制过去即可（那是主控的写入范围，本 crate 不碰）。

use std::path::PathBuf;

/// comctl32 v6 的依赖声明
///
/// 只声明 comctl32，**不**在这里写 dpiAware：DPI 感知走运行时的
/// `SetProcessDpiAwarenessContext(PER_MONITOR_AWARE_V2)`，这样宿主进程
/// 已经设过时我们能拿到 `ACCESS_DENIED` 并优雅降级，而不是被 manifest
/// 在启动前就锁死一个模式。
const MANIFEST: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0">
  <assemblyIdentity
      type="win32"
      name="SideLens.UI"
      version="1.0.0.0"
      processorArchitecture="*"/>
  <dependency>
    <dependentAssembly>
      <assemblyIdentity
          type="win32"
          name="Microsoft.Windows.Common-Controls"
          version="6.0.0.0"
          processorArchitecture="*"
          publicKeyToken="6595b64144ccf1df"
          language="*"/>
    </dependentAssembly>
  </dependency>
  <application xmlns="urn:schemas-microsoft-com:asm.v3">
    <windowsSettings>
      <longPathAware xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">true</longPathAware>
    </windowsSettings>
  </application>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <!-- Windows 10/11 -->
      <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/>
      <!-- Windows 8.1 -->
      <supportedOS Id="{1f676c76-80e1-4239-95bb-83d0f6d0da78}"/>
    </application>
  </compatibility>
</assembly>
"#;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");

    // 只在 Windows + MSVC 下动手：其它 target 上 link.exe 参数会被 rustc 拒掉
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_env = std::env::var("CARGO_CFG_TARGET_ENV").unwrap_or_default();
    if target_os != "windows" || target_env != "msvc" {
        return;
    }

    let out_dir = match std::env::var_os("OUT_DIR") {
        Some(d) => PathBuf::from(d),
        None => return,
    };
    let manifest_path = out_dir.join("sidelens-ui.manifest");
    if std::fs::write(&manifest_path, MANIFEST).is_err() {
        // 写不出来只是少一层视觉样式，不该让整个构建失败
        return;
    }

    // 路径里带空格时不能直接塞进 link.exe 参数：
    // cargo:rustc-link-arg 是**逐字**传给链接器的，加引号会被当成文件名的一部分
    // （实测 LNK1104：无法打开文件「"D:\...manifest"」，引号进了文件名）。
    // 这种情况宁可不嵌 manifest 也不要把构建搞挂 —— 界面会退回经典外观，
    // 但功能完全正常，而且 cargo:warning 会明确告诉用户为什么。
    let path_str = manifest_path.display().to_string();
    if path_str.contains(' ') {
        println!(
            "cargo:warning=sidelens-ui: 构建路径含空格，跳过 comctl32 v6 manifest 嵌入（界面将退回经典外观）：{path_str}"
        );
        return;
    }

    // 只作用于 examples：本 crate 没有 tests/ 目录，声明 rustc-link-arg-tests
    // 会让 cargo 直接报「没有 test target」。库本身没有链接阶段，不需要它。
    println!("cargo:rustc-link-arg-examples=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg-examples=/MANIFESTINPUT:{path_str}");
}
