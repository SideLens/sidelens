# ADR 0007：虚拟摄像头路线选择

- 状态：已采纳（部分实现）
- 日期：2026-11-24

## 决策

Windows 虚拟摄像头用 **MFCreateVirtualCamera**，不用 DirectShow 滤镜。

| 维度 | MFCreateVirtualCamera | DirectShow 滤镜 |
| --- | --- | --- |
| 最低系统 | Win11 Build 22000+ | 全版本 |
| 驱动 | 不需要 | 需 COM 注册（regsvr32） |
| 签名 | 不需要 | 需 Authenticode（EV 更好） |
| 用户操作 | 无 | 管理员权限 + 注册表 |
| 卸载残留 | 无（进程退出即消失） | 需手动 unregsvr32 |

**取舍**：放弃 Win10 用户，换取「零安装摩擦」。
对「插上就用」这个产品定位来说，这个交换是值得的。

## 已实现的四道门槛（实测）

| # | 门槛 | 错误码 | 状态 |
| --- | --- | --- | --- |
| 1 | 系统版本 ≥ Build 22000 | — | ✅ 通过 RtlGetVersion |
| 2 | COM(MTA) + MF 初始化 | `0x800401F0` | ✅ 通过 |
| 3 | sourceId 是 CLSID 字符串 | `0x800401F3` | ✅ 通过 |
| 4 | CLSID 已注册类工厂 | `0x80040154` | ❌ 未实现 |

每一步的错误码都不一样，且**互不包含** —— 过了前一个不代表后一个能过。
详见 [vcam-pitfalls.md](../vcam-pitfalls.md)。

## 门槛 4 的工作量估算

要实现的 COM 接口：

| 接口 | 必须 | 说明 |
| --- | --- | --- |
| `IClassFactory` | 是 | 让 `CoCreateInstance` 能创建实例 |
| `IMFMediaSource` | 是 | 媒体源本体（Start/Stop/Pause/Shutdown） |
| `IMFMediaEventGenerator` | 是 | 消费端靠事件拿流和控制响应 |
| `IMFMediaStream` | 是 | 单路视频流的请求/投递 |
| `IMFMediaType` 协商 | 是 | 分辨率/帧率/NV12 或 RGB32 |
| `IMFAttributes` | 是 | 源属性 |

`windows-core` 的 `#[implement]` 宏能省掉 `QueryInterface`/`AddRef`/`Release`
的样板，但状态机逻辑（Stopped→Started→Paused 的合法转换）省不掉。

## 降级通道

在 IMFMediaSource 实现完成前，提供两条可用路径：

1. **NDI 输出**：任何支持 NDI 的应用（OBS、vMix、Zoom 通过 NDI 插件）都能收
2. **预览窗口**：桌面端直接开窗口显示，用 OBS 的窗口采集转成摄像头

这两条都不依赖虚拟摄像头，能立刻让产品可用。
