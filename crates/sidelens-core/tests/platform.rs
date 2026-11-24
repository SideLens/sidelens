//! 平台能力矩阵与移植要点的测试
//!
//! 这些测试**不依赖宿主机**（除了第 1 条要确认当前开发环境），
//! 全部只读常量表，所以任何平台都能跑通。
//!
//! 测试的目标不是「验证代码逻辑」，而是**守住这张规格表**：
//! 矩阵改了、漏了一格、把 `Partial` 误写成 `Full`，这里要立刻红。

use sidelens_core::platform::{
    capabilities_of, evidence_for, matrix_markdown, migration_cost, migration_order,
    missing_capabilities, support_level, supports, Capability, Evidence, Platform, PlatformNotes,
    SupportLevel, MATRIX,
};

/// 全部平台（不含 Unknown），用于「除 Unknown 外都……」这类断言
const REAL_PLATFORMS: [Platform; 6] = [
    Platform::Windows,
    Platform::MacOs,
    Platform::Linux,
    Platform::Android,
    Platform::Ios,
    Platform::HarmonyOs,
];

// ---------------------------------------------------------------------------
// 1. 平台身份
// ---------------------------------------------------------------------------

/// `current()` 必须与编译目标一致
///
/// # 这条测试在修什么（第六十一轮）
///
/// 原来它硬编码 `Platform::Windows`，理由是「开发机是 Windows」。
/// 但那是**把开发环境当成了被测性质** —— 在 Linux / macOS 上跑必然失败，
/// 而失败信息看起来像「移植有问题」，实际是测试写错了。
///
/// 在 WSL 里跑 Linux 原生测试时实测：
///
/// ```text
/// left: Linux
/// right: Windows
/// ```
///
/// 这条测试的**真实意图**是「证明 `cfg!` 判定链没写反」（比如把
/// `target_os = "macos"` 的字符串打错字，或者两个分支写反）。
/// 那个意图用「按 `cfg!` 反推期望值」就能达成，且**在任何平台都成立**。
///
/// # 为什么这个写法仍然能抓到错误
///
/// 如果 `Platform::current()` 里某个 `cfg!` 分支打错字（比如
/// `target_os = "linx"`），那个分支永远不成立，函数会落到兜底分支 ——
/// 而兜底是 `Unknown`。下面的断言会立刻发现「期望 Linux 实际 Unknown」。
#[test]
fn current_platform_matches_compile_target() {
    // 按 cfg! 反推期望值 —— 与 `Platform::current()` 内部用的是同一组条件，
    // 但**独立写一遍**（不是调用它），这样两边写错任一处都会不一致。
    let expect = if cfg!(target_os = "windows") {
        Platform::Windows
    } else if cfg!(target_os = "macos") {
        Platform::MacOs
    } else if cfg!(target_os = "linux") {
        Platform::Linux
    } else if cfg!(target_os = "android") {
        Platform::Android
    } else if cfg!(target_os = "ios") {
        Platform::Ios
    } else if cfg!(target_env = "ohos") {
        Platform::HarmonyOs
    } else {
        Platform::Unknown
    };
    assert_eq!(
        Platform::current(),
        expect,
        "Platform::current() 与编译目标不一致（cfg! 判定链可能写错）"
    );
    // 兜底分支也要能被识别：如果落到 Unknown，说明所有条件都没匹配上
    assert_ne!(
        Platform::current(),
        Platform::Unknown,
        "当前编译目标未被 Platform::current() 识别 —— 检查 cfg! 字符串是否拼错"
    );
}

/// 每个平台的 `name()` 都非空，且互不重复
///
/// 名字会进日志和配置，重复会导致「按平台名查配置」时串味。
#[test]
fn every_platform_has_unique_nonempty_name() {
    let mut seen: Vec<&'static str> = Vec::new();
    for p in Platform::ALL {
        let name = p.name();
        assert!(!name.is_empty(), "{p:?} 的 name() 不能为空");
        assert!(
            !seen.contains(&name),
            "{p:?} 的 name() 与已有平台重复：{name}"
        );
        seen.push(name);
    }
    assert_eq!(seen.len(), Platform::ALL.len());
}

/// 桌面 / 移动的划分要自洽：每个平台恰好属于一边
#[test]
fn desktop_and_mobile_are_mutually_exclusive() {
    for p in Platform::ALL {
        let desktop = p.is_desktop();
        let mobile = p.is_mobile();
        assert!(!(desktop && mobile), "{p:?} 同时被判定为桌面和移动平台");
        if p == Platform::Unknown {
            assert!(!desktop && !mobile, "Unknown 既不是桌面也不是移动平台");
        } else {
            assert!(desktop || mobile, "{p:?} 既不是桌面也不是移动平台");
        }
    }
}

// ---------------------------------------------------------------------------
// 2. 能力矩阵的关键格子
// ---------------------------------------------------------------------------

/// 定时器：只有 Windows 需要（也才有）提精度的手段
///
/// macOS / Linux 的 tokio 定时器本来就是微秒级，不需要 `timeBeginPeriod` 那种东西。
#[test]
fn high_res_timer_is_windows_only() {
    assert!(supports(Platform::Windows, Capability::HighResTimer));
    assert!(!supports(Platform::MacOs, Capability::HighResTimer));
    assert!(!supports(Platform::Linux, Capability::HighResTimer));

    // 桌面三家里只有 Windows 是 Full，另外两家是 None 而不是 Partial ——
    // 「不需要」和「做不到」在能力矩阵里都记 None，因为对调用方而言结果一样：
    // 不用调那个 API。
    assert_eq!(
        support_level(Platform::Windows, Capability::HighResTimer),
        SupportLevel::Full
    );
    assert_eq!(
        support_level(Platform::MacOs, Capability::HighResTimer),
        SupportLevel::None
    );
}

/// 虚拟摄像头：只有 Windows 有系统级 API
///
/// macOS 要写 DriverKit 扩展、Linux 要装 v4l2loopback，
/// 两者都不是「平台 API」，所以矩阵里是 None —— 这是移植工作量的主要来源。
#[test]
fn virtual_camera_api_is_windows_only() {
    assert!(supports(Platform::Windows, Capability::VirtualCameraApi));
    assert!(!supports(Platform::MacOs, Capability::VirtualCameraApi));
    assert!(!supports(Platform::Linux, Capability::VirtualCameraApi));

    for p in REAL_PLATFORMS {
        if p != Platform::Windows {
            assert_eq!(
                support_level(p, Capability::VirtualCameraApi),
                SupportLevel::None,
                "{p:?} 不应被标记为有系统级虚拟摄像头 API"
            );
        }
    }
}

/// 所有桌面平台都有共享内存映射
///
/// 这条很关键：3.1MB 的 1080p NV12 帧不可能走管道传，
/// 只要共享内存这条能力断了，整条帧传输链路在目标平台上就废了。
#[test]
fn all_desktop_platforms_support_shared_memory() {
    for p in REAL_PLATFORMS {
        if p.is_desktop() {
            assert_eq!(
                support_level(p, Capability::SharedMemory),
                SupportLevel::Full,
                "{p:?} 是桌面平台，必须有共享内存映射"
            );
        }
    }
}

/// 除 Unknown 外所有平台都有硬件 H.264
///
/// 硬编是实时推流的底线：软编在移动端会直接烧穿电量预算。
/// 这条不成立的话，对应平台要先解决编码器才能谈移植。
#[test]
fn every_real_platform_has_hardware_h264() {
    for p in REAL_PLATFORMS {
        assert!(
            supports(p, Capability::HardwareH264),
            "{p:?} 应当有硬件 H.264 编码器"
        );
    }
    assert_eq!(
        support_level(Platform::Unknown, Capability::HardwareH264),
        SupportLevel::None,
        "Unknown 不应声称有硬件编码器"
    );
}

/// `supports` 与 `support_level != None` 必须完全一致
///
/// 遍历全部 7 × 9 = 63 个组合。这条测试的价值在于：
/// 将来有人为了「临时放行」在 `supports` 里加个特判，
/// 三态与二态就会分叉，这里会立刻红。
#[test]
fn supports_matches_support_level_for_all_combinations() {
    let mut checked = 0;
    for p in Platform::ALL {
        for cap in Capability::ALL {
            let level = support_level(p, cap);
            let expected = level != SupportLevel::None;
            assert_eq!(
                supports(p, cap),
                expected,
                "supports({p:?}, {cap:?}) 与 support_level({level:?}) 不一致"
            );
            checked += 1;
        }
    }
    assert_eq!(
        checked,
        Platform::ALL.len() * Capability::ALL.len(),
        "没有遍历完所有组合"
    );
}

/// macOS 的 NamedPipe 是 Full：Unix domain socket 就算命名管道
///
/// 语义上两者都是「本机命名寻址的字节流通道」，对本项目而言可等价替换，
/// 所以记 Full 而不是 Partial —— 移植时这块不需要降级设计。
#[test]
fn macos_named_pipe_is_full() {
    assert_eq!(
        support_level(Platform::MacOs, Capability::NamedPipe),
        SupportLevel::Full
    );
    assert_eq!(
        support_level(Platform::Linux, Capability::NamedPipe),
        SupportLevel::Full
    );
    // 移动端只有抽象 socket，语义与 AF_UNIX 不完全等价，降为 Partial
    assert_eq!(
        support_level(Platform::Android, Capability::NamedPipe),
        SupportLevel::Partial
    );
}

/// Android 的 UDP 打洞是 Partial：能做，但移动网络下成功率明显下降
///
/// 这是「三态」存在的理由：把它压成 `true` 会让上层以为可以只靠打洞，
/// 压成 `false` 又会白白放弃 WiFi 场景下完全可用的路径。
#[test]
fn android_udp_hole_punching_is_partial() {
    assert_eq!(
        support_level(Platform::Android, Capability::UdpHolePunching),
        SupportLevel::Partial
    );
    // Partial 仍算「支持」，因为功能做得出来
    assert!(supports(Platform::Android, Capability::UdpHolePunching));

    for p in [Platform::Windows, Platform::MacOs, Platform::Linux] {
        assert_eq!(
            support_level(p, Capability::UdpHolePunching),
            SupportLevel::Full,
            "{p:?} 的 UDP 打洞应当是 Full"
        );
    }
}

// ---------------------------------------------------------------------------
// 3. 矩阵本身的完整性
// ---------------------------------------------------------------------------

/// 矩阵行序必须与 `Platform::ALL` 一致
///
/// 这是整张表唯一的隐式耦合：`platform_index` 把 `Platform` 映射成下标，
/// 而矩阵的列是裸数组。两者一旦错位，所有结论都会整体串位，
/// 而且不会有任何编译错误 —— 所以必须靠测试守住。
#[test]
fn matrix_platform_order_matches_all() {
    // 用几组「特征格子」反查下标：这些格子在矩阵里是唯一的
    // Windows 的 HighResTimer 是 Full，其他平台都不是
    let row = MATRIX
        .iter()
        .find(|(cap, _)| *cap == Capability::HighResTimer)
        .map(|(_, row)| row)
        .expect("矩阵里必须有 HighResTimer 行");
    let windows_idx = Platform::ALL
        .iter()
        .position(|p| *p == Platform::Windows)
        .expect("Platform::ALL 里必须有 Windows");
    assert_eq!(
        row[windows_idx],
        SupportLevel::Full,
        "Platform::ALL 里 Windows 的下标，必须对上矩阵里 HighResTimer=Full 的那一列"
    );

    // 逐格复核：所有平台 × 所有能力，用 ALL 的下标查矩阵必须等于 support_level
    for p in Platform::ALL {
        let idx = Platform::ALL.iter().position(|q| *q == p).unwrap();
        for cap in Capability::ALL {
            let (_, row) = MATRIX
                .iter()
                .find(|(c, _)| *c == cap)
                .expect("矩阵里缺能力行");
            assert_eq!(
                row[idx],
                support_level(p, cap),
                "矩阵列下标与 Platform::ALL 不一致：{p:?} / {cap:?}"
            );
        }
    }
}

/// 矩阵必须覆盖每一个 (平台, 能力) 组合，且每行只有一个来源
#[test]
fn matrix_is_complete_and_has_no_duplicate_rows() {
    assert_eq!(
        MATRIX.len(),
        Capability::ALL.len(),
        "矩阵行数必须等于能力数"
    );
    for (i, (cap, _)) in MATRIX.iter().enumerate() {
        assert_eq!(*cap, Capability::ALL[i], "矩阵第 {i} 行的能力顺序不对");
    }
    // 没有重复行：用迭代器而不是下标，避免「用 i 索引 MATRIX」这类误写
    let mut seen: Vec<Capability> = Vec::new();
    for (cap, _) in &MATRIX {
        assert!(!seen.contains(cap), "能力 {cap:?} 在矩阵里出现了两次");
        seen.push(*cap);
    }
}

/// Unknown 平台对任何能力都是 None（保守失败）
///
/// 「不知道」绝不能当成「支持」：那会让代码在真机上调用不存在的 API。
#[test]
fn unknown_platform_supports_nothing() {
    for cap in Capability::ALL {
        assert_eq!(
            support_level(Platform::Unknown, cap),
            SupportLevel::None,
            "Unknown 平台不应声称支持 {cap:?}"
        );
    }
    assert!(capabilities_of(Platform::Unknown).is_empty());
    assert_eq!(
        missing_capabilities(Platform::Unknown).len(),
        Capability::ALL.len()
    );
}

/// `capabilities_of` 与 `missing_capabilities` 是完整划分
#[test]
fn capabilities_partition_is_exhaustive() {
    for p in Platform::ALL {
        let have = capabilities_of(p);
        let lack = missing_capabilities(p);
        assert_eq!(
            have.len() + lack.len(),
            Capability::ALL.len(),
            "{p:?} 的支持/缺失清单加起来不等于全部能力"
        );
        for cap in Capability::ALL {
            assert!(
                have.contains(&cap) ^ lack.contains(&cap),
                "{cap:?} 在 {p:?} 上必须恰好属于支持或缺失之一"
            );
        }
    }
}

/// 每条能力都要有可查证的实现载体（providers 不能全是空串）
#[test]
fn every_capability_has_provider_notes() {
    for cap in Capability::ALL {
        let providers = cap.providers();
        assert_eq!(
            providers.len(),
            Platform::ALL.len(),
            "{cap:?} 的 providers 长度必须等于平台数"
        );
        for (i, text) in providers.iter().enumerate() {
            assert!(
                !text.is_empty(),
                "{cap:?} 在 {:?} 上缺少实现载体说明",
                Platform::ALL[i]
            );
        }
        // Unknown 的实现载体恒为「无」
        assert_eq!(
            providers[6], "无",
            "{cap:?} 的 Unknown 列应写「无」而不是留空"
        );
    }
}

/// 硬件编码的证据等级是 DeviceDependent，其余是 Verified
///
/// 这条把「哪些结论可以直接排期」写死：硬件编码要按机型确认，
/// 不能拿平台级结论去承诺「一定有硬编」。
#[test]
fn evidence_levels_are_explicit() {
    for cap in Capability::ALL {
        let expected = match cap {
            Capability::HardwareH264 | Capability::HardwareH265 => Evidence::DeviceDependent,
            _ => Evidence::Verified,
        };
        assert_eq!(cap.evidence(), expected, "{cap:?} 的证据等级不对");
    }
}

/// 生成的 Markdown 必须包含全部平台名与能力名
#[test]
fn matrix_markdown_covers_all_rows_and_columns() {
    let md = matrix_markdown();
    for p in Platform::ALL {
        assert!(md.contains(p.name()), "Markdown 表头缺少平台 {}", p.name());
    }
    for cap in Capability::ALL {
        assert!(md.contains(cap.name()), "Markdown 缺少能力 {}", cap.name());
    }
    // 表头 + 分隔行 + 9 行能力
    assert_eq!(
        md.lines().count(),
        2 + Capability::ALL.len(),
        "Markdown 表行数不对"
    );
}

// ---------------------------------------------------------------------------
// 4. 移植要点
// ---------------------------------------------------------------------------

/// 每个平台都必须给出非空的移植清单
///
/// 包括 Unknown —— 它的 todo 是「先确定平台」，这本身就是一件必须做的事。
/// 空清单会让移植计划看起来「没什么可做」，是危险的假象。
#[test]
fn every_platform_has_nonempty_notes() {
    for p in Platform::ALL {
        let notes = PlatformNotes::for_platform(p);
        assert_eq!(notes.platform, p, "{p:?} 的 notes 平台字段对不上");
        assert!(!notes.todo.is_empty(), "{p:?} 的 todo 不能为空");
        assert!(!notes.pitfalls.is_empty(), "{p:?} 的 pitfalls 不能为空");
        for item in notes.todo.iter().chain(notes.pitfalls.iter()) {
            assert!(!item.trim().is_empty(), "{p:?} 的要点里不能有空串");
        }
    }
}

/// macOS 的坑里必须提到 DriverKit
///
/// 这是 macOS 移植的头号风险：虚拟摄像头没有系统 API，
/// 必须写 DriverKit 扩展，而 entitlement 要 Apple 审批。
#[test]
fn macos_pitfalls_mention_driverkit() {
    let notes = PlatformNotes::for_platform(Platform::MacOs);
    let joined = notes.pitfalls.join("\n");
    assert!(
        joined.contains("DriverKit"),
        "macOS 的 pitfalls 必须提到 DriverKit，实际内容：\n{joined}"
    );
    // todo 里也要有对应的工作项
    let todo = notes.todo.join("\n");
    assert!(
        todo.contains("DriverKit"),
        "macOS 的 todo 里也要列出 DriverKit 扩展这项工作"
    );
}

/// Linux 的坑里必须提到 v4l2loopback
///
/// 这是 Linux 移植的安装摩擦来源：内核模块要用户自己装、自己 modprobe。
#[test]
fn linux_pitfalls_mention_v4l2loopback() {
    let notes = PlatformNotes::for_platform(Platform::Linux);
    let joined = notes.pitfalls.join("\n");
    assert!(
        joined.contains("v4l2loopback"),
        "Linux 的 pitfalls 必须提到 v4l2loopback，实际内容：\n{joined}"
    );
    let todo = notes.todo.join("\n");
    assert!(
        todo.contains("v4l2loopback"),
        "Linux 的 todo 里也要列出 v4l2loopback 检测"
    );
}

/// 移动端的坑必须覆盖生命周期与后台限制
///
/// 桌面背景下的「进程一直在跑」在移动端不成立，
/// 这是移植时最容易低估的一类工作。
#[test]
fn mobile_pitfalls_cover_lifecycle() {
    let android = PlatformNotes::for_platform(Platform::Android)
        .pitfalls
        .join("\n");
    assert!(
        android.contains("前台 Service"),
        "Android 的坑必须提到前台 Service"
    );
    assert!(android.contains("NAT"), "Android 的坑必须提到移动网络 NAT");
    assert!(android.contains("发热"), "Android 的坑必须提到发热/电量");

    let ios = PlatformNotes::for_platform(Platform::Ios)
        .pitfalls
        .join("\n");
    assert!(ios.contains("后台"), "iOS 的坑必须提到后台运行限制");
    assert!(ios.contains("ReplayKit"), "iOS 的坑必须提到 ReplayKit");

    let harmony = PlatformNotes::for_platform(Platform::HarmonyOs)
        .pitfalls
        .join("\n");
    assert!(
        harmony.contains("待验证"),
        "HarmonyOS 的坑必须显式标注待验证项"
    );
}

/// 证据等级按平台细化：HarmonyOS 与 Unknown 的结论都还没验证过
///
/// 这条把「哪些格子可以直接拿去排期」写死。HarmonyOS 列里那些
/// Partial 若被当成已确认结论，移植计划会在第一步就崩。
#[test]
fn evidence_for_marks_unverified_platforms() {
    for cap in Capability::ALL {
        assert_eq!(
            evidence_for(Platform::HarmonyOs, cap),
            Evidence::Unverified,
            "HarmonyOS / {cap:?} 尚未真机验证，不能标成已查证"
        );
        assert_eq!(
            evidence_for(Platform::Unknown, cap),
            Evidence::Unverified,
            "Unknown / {cap:?} 什么都不知道"
        );
        // 已查证的五个平台：除硬件编码外都是 Verified
        for p in REAL_PLATFORMS {
            if p == Platform::HarmonyOs {
                continue;
            }
            let expected = match cap {
                Capability::HardwareH264 | Capability::HardwareH265 => Evidence::DeviceDependent,
                _ => Evidence::Verified,
            };
            assert_eq!(
                evidence_for(p, cap),
                expected,
                "{p:?} / {cap:?} 的证据等级不对"
            );
        }
    }
}

/// 移植顺序：按加权代价升序，Windows 最省事，Unknown 不参与
#[test]
fn migration_order_starts_with_windows_and_excludes_unknown() {
    let order = migration_order();
    assert_eq!(
        order.len(),
        Platform::ALL.len() - 1,
        "移植顺序里应排除 Unknown"
    );
    assert!(!order.contains(&Platform::Unknown), "Unknown 不是目标平台");
    assert_eq!(order[0], Platform::Windows, "Windows 是现状平台，代价最低");

    // 必须按加权代价单调不减
    let mut prev = 0;
    for p in &order {
        let cost = migration_cost(*p);
        assert!(
            cost >= prev,
            "{p:?} 的代价（{cost}）小于前一个平台（{prev}），排序失效"
        );
        prev = cost;
    }
    // 每个真实平台都恰好出现一次
    for p in REAL_PLATFORMS {
        assert_eq!(
            order.iter().filter(|q| **q == p).count(),
            1,
            "{p:?} 在移植顺序里应恰好出现一次"
        );
    }
}

/// 加权代价：只数 None 会让排序退化，Partial 必须计入
///
/// 六个非 Windows 平台的 `None` 数量都是 3，如果只数 None，
/// 排序就等于枚举声明顺序，没有信息量。这条测试锁住「Partial 计 1 分」。
#[test]
fn migration_cost_counts_partial_support() {
    // Windows 全是 Full，代价为 0
    assert_eq!(migration_cost(Platform::Windows), 0);

    // 桌面两家：3 个 None，没有 Partial
    assert_eq!(migration_cost(Platform::MacOs), 6);
    assert_eq!(migration_cost(Platform::Linux), 6);

    // 移动端：3 个 None + 2 个 Partial（命名管道、UDP 打洞）
    //
    // 屏幕捕获在 Android / iOS 上是 Full（MediaProjection / ReplayKit），
    // 所以 Partial 只有两项 —— 这个数字变了就说明矩阵被改过，要重新评估。
    for p in [Platform::Android, Platform::Ios] {
        let none = missing_capabilities(p).len();
        assert_eq!(none, 3, "{p:?} 的 None 项应当是 3");
        let partial = partial_count(p);
        assert_eq!(
            partial, 2,
            "{p:?} 的 Partial 项应当是 2（命名管道 + UDP 打洞）"
        );
        let cost = migration_cost(p);
        assert_eq!(cost, none * 2 + partial, "{p:?} 的代价计算与矩阵不一致");
        assert!(
            cost > migration_cost(Platform::MacOs),
            "{p:?} 有 Partial 项，代价必须高于桌面平台"
        );
    }

    // HarmonyOS 的 Partial 最多，代价最高
    assert!(
        migration_cost(Platform::HarmonyOs) >= migration_cost(Platform::Ios),
        "HarmonyOS 的待验证/部分支持项最多，代价不应低于 iOS"
    );

    // Unknown 全是 None
    assert_eq!(migration_cost(Platform::Unknown), Capability::ALL.len() * 2);
}

/// 某平台的 Partial 数量（测试辅助）
fn partial_count(p: Platform) -> usize {
    Capability::ALL
        .iter()
        .filter(|cap| support_level(p, **cap) == SupportLevel::Partial)
        .count()
}

/// 缺失能力数应当与矩阵一致，且 Windows 最少
///
/// 这是移植优先级的粗排序依据：缺失越少，改造越小。
#[test]
fn missing_count_reflects_matrix() {
    let windows = PlatformNotes::for_platform(Platform::Windows).missing_count();
    for p in REAL_PLATFORMS {
        if p == Platform::Windows {
            continue;
        }
        let other = PlatformNotes::for_platform(p).missing_count();
        assert!(
            other >= windows,
            "{p:?} 的缺失能力数（{other}）不应少于 Windows（{windows}）"
        );
    }
    assert_eq!(
        PlatformNotes::for_platform(Platform::Unknown).missing_count(),
        Capability::ALL.len()
    );
}
