//! 给 `sidelensd.exe` 嵌 comctl32 v6 的 manifest
//!
//! # 为什么 apps/ 下要自己再来一份
//!
//! `sidelens-ui` 的 `build.rs` 只给**它自己的 examples** 嵌了 manifest。
//! 库没有链接阶段，`cargo:rustc-link-arg` 对库无效也无害 —— 真正需要
//! 视觉样式的是**可执行文件**，也就是这里的 `sidelensd.exe`。
//!
//! 不嵌的后果：进程加载的是 comctl32 **v5**，按钮/复选框/下拉框退回
//! Win95 经典外观（方块灰、无圆角、标题栏深蓝渐变）。用户要的是
//! 「极致的原生 UI 体验」，经典外观在 200% 缩放下连字都会糊。
//!
//! # 为什么用链接器参数而不是 `rc.exe`
//!
//! 常规做法是写 `.rc` 用 `rc.exe` 编译成 `.res` 再链进去，但那要求构建机
//! 上有 Windows SDK 的 `rc.exe`（cargo 不会帮你找路径）。MSVC 的 `link.exe`
//! 自带 `/MANIFESTINPUT:` 可以直接吃 XML 并合并进 PE 的 RT_MANIFEST 资源 ——
//! 零外部工具、零第三方 crate。

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
      name="SideLens.Daemon"
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
      <supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}"/>
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
    let manifest_path = out_dir.join("sidelensd.manifest");
    if std::fs::write(&manifest_path, MANIFEST).is_err() {
        return;
    }

    // 路径里带空格时不能直接塞进 link.exe 参数：
    // `cargo:rustc-link-arg` 是**逐字**传给链接器的，加引号会被当成文件名的
    // 一部分（实测 LNK1104：无法打开文件「"D:\...manifest"」，引号进了文件名）。
    let path_str = manifest_path.display().to_string();
    if path_str.contains(' ') {
        println!(
            "cargo:warning=sidelensd: 构建路径含空格，跳过 comctl32 v6 manifest 嵌入（界面将退回经典外观）：{path_str}"
        );
        return;
    }

    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg-bins=/MANIFESTINPUT:{path_str}");
}
