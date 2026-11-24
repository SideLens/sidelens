//! 验证 H.264 编码输出能被 nalu / sps 层正确解析
//!
//! # 为什么要单独验证
//!
//! 「编码器输出了字节」与「输出的是合法 Annex-B」是两件事。
//! MF 的 H.264 编码器到底吐 Annex-B 还是 AVCC 没有文档保证 ——
//! 猜错了整条链路的 `is_keyframe` 都会失效，表现为「偶尔花屏」这种最难查的症状。
//!
//! 所以必须用**独立的解析器**（sidelens-codec 的 nalu + sps）
//! 反过来验证编码器的输出。

// 整个实现是 Windows 专属（Media Foundation）。
// 用 mod imp 包一层而不是 #![cfg(windows)] —— 后者会把 main 一起删掉，
// 报 E0601「main function not found」，看起来像忘了写 main。
#[cfg(windows)]
mod imp {
    use std::env;

    use sidelens_codec::nalu::{split_annexb, NaluType, ParameterSets};
    use sidelens_codec::sps::SpsInfo;

    pub fn main() {
        let path = env::args()
            .nth(1)
            .unwrap_or_else(|| "target/test.h264".into());
        let data = std::fs::read(&path).expect("读不到 dump 文件");

        println!("文件：{path}（{} 字节）", data.len());

        // 起始码统计
        let n4 = data.windows(4).filter(|w| w == &[0, 0, 0, 1]).count();
        let n3 = data.windows(3).filter(|w| w == &[0, 0, 1]).count();
        println!("起始码：4 字节 {n4} 个，3 字节 {n3} 个（3 字节含 4 字节的子串）");

        let nalus = split_annexb(&data);
        println!("切出 {} 个 NALU", nalus.len());

        // 按类型统计
        let mut sps = 0;
        let mut pps = 0;
        let mut idr = 0;
        let mut slice = 0;
        let mut sei = 0;
        let mut aud = 0;
        let mut other = 0;
        for n in &nalus {
            match n.ty {
                NaluType::Sps => sps += 1,
                NaluType::Pps => pps += 1,
                NaluType::IdrSlice => idr += 1,
                NaluType::NonIdrSlice => slice += 1,
                NaluType::Sei => sei += 1,
                NaluType::Aud => aud += 1,
                NaluType::Other(_) => other += 1,
            }
        }
        println!("类型：SPS {sps} / PPS {pps} / IDR {idr} / 非IDR切片 {slice} / SEI {sei} / AUD {aud} / 其他 {other}");

        // 参数集解析
        let mut ps = ParameterSets::new();
        ps.update_from(&data);
        match ps.sps() {
            Some(s) => match SpsInfo::parse(s) {
                Ok(info) => {
                    println!("SPS 解析成功：");
                    println!(
                        "  profile_idc = {} / level_idc = {}",
                        info.profile_idc, info.level_idc
                    );
                    println!("  coded_resolution = {:?}", info.coded_resolution());
                    println!("  resolution       = {:?}", info.resolution());
                    println!("  frame_rate       = {:?}", info.frame_rate());
                }
                Err(e) => println!("❌ SPS 解析失败：{e}"),
            },
            None => println!("❌ 没有提取到 SPS"),
        }
        println!(
            "PPS 提取：{}",
            if ps.pps().is_some() {
                "成功"
            } else {
                "失败"
            }
        );

        // 关键帧判断
        let kf = sidelens_codec::nalu::is_keyframe(&data);
        println!("is_keyframe(整个文件) = {kf}");

        // 逐 NALU 大小分布
        let mut sizes: Vec<usize> = nalus.iter().map(|n| n.data.len()).collect();
        sizes.sort_unstable();
        if !sizes.is_empty() {
            println!(
                "NALU 大小：min {} / 中位 {} / max {}",
                sizes[0],
                sizes[sizes.len() / 2],
                sizes[sizes.len() - 1]
            );
        }

        // 断言
        assert!(sps > 0, "没有 SPS —— 输出可能不是 Annex-B");
        assert!(pps > 0, "没有 PPS");
        assert!(idr > 0, "没有 IDR 帧 —— 关键帧判断会全失效");
        println!("\n✅ 全部检查通过");
    }
}

#[cfg(windows)]
fn main() {
    imp::main();
}

#[cfg(not(windows))]
fn main() {
    eprintln!("此示例仅支持 Windows —— Media Foundation 是 Windows 专属能力");
}
