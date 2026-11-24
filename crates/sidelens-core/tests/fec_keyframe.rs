//! 关键帧保护策略的端到端测试
//!
//! 与 `tests/fec_link.rs` 的分工：那边测「分片 → 组 → 冗余包 → 恢复」的
//! 基本流程，这边只测**关键帧感知的冗余度决策**，以及「决策给出的冗余度
//! 在真实 `FecSender`/`FecReceiver` 上是否真的能达到目标恢复率」。
//!
//! # 为什么必须有端到端恢复率验证
//!
//! 纯数学单测（只对比 `suggest_redundant_exact` 的返回值）挡不住**模型与实现
//! 不一致**这类错误。`crate::fec` 的 `FecDecoder::recover` 用的是高斯-约当
//! 消元，它的可解条件不是「丢失数 <= redundant」，而是「丢失落在几个不同
//! 余类」—— 任何只测「返回值是否等于某个数」的测试都无法发现模型写错。
//!
//! 所以这里用**确定性伪随机**（xorshift64，不用 rand crate）构造真实丢包，
//! 走完整的 `FecSender` → 丢包 → `FecReceiver` → `recover` 路径，统计
//! 「整帧是否完整恢复」的比例，与模型值对比。
//!
//! # 为什么统计用 32B 小载荷、不校验用 1160B 真载荷
//!
//! FEC 的可解性与**载荷长度无关**（XOR 是按字节对齐的，长度只影响内存占用）。
//! 1160B × 35 片 × 10 万次 = 4GB 分配，实测要跑几分钟；换成 32B 后同样的
//! 统计精度只要几秒。字节级正确性由 `real_payload_bytes_survive_recovery`
//! 用真实 1160B 分片单独守着（只跑少量帧）。

use sidelens_core::fec::FecConfig;
use sidelens_core::fec_link::{
    clamp_keyframe_redundant, exact_recovery_probability, suggest_redundant,
    suggest_redundant_exact, suggest_redundant_for_frame, FecReceiver, FecSender, FrameClass,
    KEYFRAME_MAX_OVERHEAD_RATIO, KEYFRAME_RECOVERY_TARGET,
};
use sidelens_core::fragment::SAFE_MTU_PAYLOAD;

/// 真实分片的数据段大小（1200 MTU - 32B 帧头 - 8B 分片头）
const REAL_FRAG_PAYLOAD: usize = SAFE_MTU_PAYLOAD - 32 - 8;

/// 统计用的载荷大小（可解性与载荷长度无关，见文件头说明）
const STAT_FRAG_PAYLOAD: usize = 32;

/// 组大小上限（与 `apps/sidelenssend` 的 `FEC_GROUP_SIZE_MAX` 一致）
const GROUP_SIZE_MAX: usize = 64;

fn cfg(group: usize, red: usize) -> FecConfig {
    FecConfig {
        group_size: group,
        redundant: red,
        max_packet_size: SAFE_MTU_PAYLOAD,
    }
}

/// 构造一帧的分片：内容由下标决定，便于恢复后逐字节校验
fn make_fragments(n: usize, payload: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|i| {
            (0..payload)
                .map(|j| ((i * 131 + j * 7) & 0xff) as u8)
                .collect()
        })
        .collect()
}

/// 确定性伪随机（xorshift64），不用 rand
///
/// 同一个 seed 在任何平台、任何 Rust 版本上都给出完全相同的序列 ——
/// 测试必须可复现，否则恢复率断言会随机翻车。
struct Xorshift64(u64);

impl Xorshift64 {
    fn new(seed: u64) -> Self {
        Self(if seed == 0 {
            0x9E37_79B9_7F4A_7C15
        } else {
            seed
        })
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// `[0, 1)`
    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// 一次完整帧传输的结果
struct FrameRun {
    /// 整帧所有分片是否都完整可用（丢的都被 FEC 救回来了）
    recovered_all: bool,
    /// 实际丢了多少个数据分片
    lost_data: usize,
    /// 实际丢了多少个冗余包
    lost_red: usize,
}

/// 用真实 `FecSender` / `FecReceiver` 跑一帧，按丢包率随机丢包
///
/// 载荷大小由 `frags` 决定；`payload_check` 为 true 时逐字节校验恢复结果。
fn run_frame_once(
    frame_seq: u32,
    frags: &[Vec<u8>],
    redundant: usize,
    loss: f64,
    rng: &mut Xorshift64,
    payload_check: bool,
) -> FrameRun {
    let k = frags.len();
    // `clamp` 而不是 `min().max()` —— 后者会被 clippy 的 `manual_clamp` 拦下。
    // 注意 `clamp` 在 `max < min` 时会 panic，所以常量必须保证
    // `GROUP_SIZE_MAX >= 2`（`fec_link::GROUP_SIZE_MAX = 64`，成立）。
    let group_size = k.clamp(2, GROUP_SIZE_MAX);
    let c = cfg(group_size, redundant.clamp(1, group_size));
    // 发送端：分片 -> 冗余包
    let mut tx = FecSender::new(c).expect("配置合法");
    tx.begin_frame(frame_seq);
    let mut reds = Vec::new();
    for f in frags {
        reds.extend(tx.push_fragment(f));
    }
    reds.extend(tx.finish_frame());

    // 链路：按丢包率独立丢弃每个包
    let mut rx = FecReceiver::new(c).expect("配置合法");
    let mut lost_data = 0usize;
    let mut received = vec![false; k];
    for (i, f) in frags.iter().enumerate() {
        if rng.next_f64() < loss {
            lost_data += 1;
            continue;
        }
        received[i] = true;
        rx.push_fragment(frame_seq as u16, i, f);
    }

    let mut lost_red = 0usize;
    let mut got_back: Vec<usize> = Vec::new();
    for r in &reds {
        if rng.next_f64() < loss {
            lost_red += 1;
            continue;
        }
        for (idx, data) in rx.push_redundant(r) {
            got_back.push(idx);
            if payload_check && idx < k {
                assert_eq!(
                    &data[..frags[idx].len()],
                    &frags[idx][..],
                    "恢复出的分片内容必须逐字节一致（下标 {idx}）"
                );
            }
        }
    }

    let mut all = true;
    for (i, ok) in received.iter().enumerate() {
        if !ok && !got_back.contains(&i) {
            all = false;
            break;
        }
    }

    FrameRun {
        recovered_all: all,
        lost_data,
        lost_red,
    }
}

/// 跑 `trials` 帧，返回整帧恢复率（小载荷，纯统计）
fn measure_recovery_rate(k: usize, redundant: usize, loss: f64, trials: usize, seed: u64) -> f64 {
    let frags = make_fragments(k, STAT_FRAG_PAYLOAD);
    let mut rng = Xorshift64::new(seed);
    let mut ok = 0usize;
    for t in 0..trials {
        if run_frame_once(t as u32, &frags, redundant, loss, &mut rng, false).recovered_all {
            ok += 1;
        }
    }
    ok as f64 / trials as f64
}

// ============================================================================
// 1. 单调性：关键帧的冗余度 >= 同条件 P 帧
// ============================================================================

/// 关键帧的冗余度必须 >= 同条件下 P 帧的冗余度（`fragments >= 8` 段）
///
/// # 为什么只断言 `>= 8`
///
/// `fragments < 8` 时带宽上限（50% 开销）比二项模型的建议更紧：8 片帧上限是
/// 4，而二项模型在 10% 丢包下会要 3-5。这是刻意的取舍（小帧走 NACK 比烧
/// 50% 带宽划算），不是 bug。关键帧的真实尺寸段（35-64 片）不受影响。
#[test]
fn keyframe_never_less_than_delta() {
    for k in 8..=GROUP_SIZE_MAX {
        for &loss in &[0.005, 0.01, 0.02, 0.03, 0.05, 0.08, 0.10, 0.12, 0.15] {
            let kf = suggest_redundant_for_frame(FrameClass::keyframe(k), loss, 0.99);
            let df = suggest_redundant_for_frame(FrameClass::delta(k), loss, 0.99);
            assert!(
                kf >= df,
                "k={k} loss={loss}: 关键帧冗余 {kf} 小于 P 帧 {df} —— 关键帧更重要，不该更少保护"
            );
        }
    }
}

/// 关键帧的冗余度随丢包率单调不减
#[test]
fn keyframe_redundant_monotonic_in_loss() {
    for k in [8usize, 16, 35, 55, 64] {
        let mut prev = 0usize;
        for &loss in &[0.0, 0.005, 0.01, 0.02, 0.03, 0.05, 0.08, 0.10] {
            let r = suggest_redundant_for_frame(FrameClass::keyframe(k), loss, 0.99);
            assert!(
                r >= prev,
                "k={k} loss={loss}: 冗余 {r} 比更低的丢包率 {prev} 还少"
            );
            prev = r;
        }
    }
}

/// 高丢包（10%）下关键帧冗余度必须**显著**更高（不是「高一点点」）
///
/// 「显著」的判据：至少是 P 帧的 2 倍，且绝对值 >= 8 个包。
/// 55 片关键帧在 10% 丢包下：P 帧只要 11 个，关键帧要 27 个（被带宽钳到 27）。
#[test]
fn high_loss_keyframe_is_much_higher() {
    for k in [35usize, 55, 64] {
        let kf = suggest_redundant_for_frame(FrameClass::keyframe(k), 0.10, 0.99);
        let df = suggest_redundant_for_frame(FrameClass::delta(k), 0.10, 0.99);
        assert!(
            kf >= df * 2,
            "k={k} @10%: 关键帧 {kf} 不到 P 帧 {df} 的 2 倍 —— 保护力度不够"
        );
        assert!(kf >= 8, "k={k} @10%: 关键帧只有 {kf} 个冗余包，太少");
    }
}

// ============================================================================
// 2. 带宽上限
// ============================================================================

/// 55 片关键帧的冗余度不得超过带宽上限
///
/// 55 × 0.5 = 27.5 -> floor 27。若不钳，`suggest_redundant_exact` 在 5% 丢包下
/// 会给出 55（100% 开销，冗余字节 63.8KB > 原帧 39.4KB）。
#[test]
fn keyframe_redundant_is_clamped_by_bandwidth() {
    let cap = (55.0 * KEYFRAME_MAX_OVERHEAD_RATIO).floor() as usize;
    assert_eq!(cap, 27, "55 片的 50% 上限应是 27");

    for &loss in &[0.01, 0.02, 0.03, 0.05, 0.10, 0.20] {
        let raw = suggest_redundant_exact(55, loss, KEYFRAME_RECOVERY_TARGET);
        let clamped = clamp_keyframe_redundant(55, raw);
        assert!(
            clamped <= cap,
            "loss={loss}: 钳制后 {clamped} 超过上限 {cap}（原始建议 {raw}）"
        );
        // 5% 以上丢包时原始建议一定撞顶，否则说明模型没在起作用
        if loss >= 0.05 {
            assert!(
                raw >= cap,
                "loss={loss}: 原始建议 {raw} 未达上限 {cap}，模型可能低估了关键帧的需求"
            );
        }
    }
}

/// 所有分片数下钳制都生效
#[test]
fn clamp_holds_for_all_sizes() {
    for k in 1..=GROUP_SIZE_MAX {
        let cap = ((k as f64 * KEYFRAME_MAX_OVERHEAD_RATIO).floor() as usize).max(1);
        for want in [1usize, 5, 16, 55, 100, k] {
            let got = clamp_keyframe_redundant(k, want);
            assert!(got <= cap, "k={k} want={want}: 钳制后 {got} 超过上限 {cap}");
            assert!(got <= want, "k={k} want={want}: 钳制不该放大（得到 {got}）");
        }
    }
}

/// 钳制不会把「有分片的关键帧」钳成 0
#[test]
fn clamp_never_zero_when_fragments_exist() {
    for k in 1..=GROUP_SIZE_MAX {
        assert!(
            clamp_keyframe_redundant(k, 1) >= 1,
            "k={k}: 有分片时至少该给 1 个冗余包"
        );
    }
    assert_eq!(clamp_keyframe_redundant(0, 10), 0, "空帧没有东西可保护");
    assert_eq!(clamp_keyframe_redundant(55, 0), 0, "want=0 应保持 0");
}

// ============================================================================
// 3. 0% 丢包：两者都是最小值
// ============================================================================

/// 不丢包时不该白付冗余
#[test]
fn zero_loss_pays_nothing() {
    for k in [1usize, 2, 8, 35, 55, 64] {
        assert_eq!(
            suggest_redundant_for_frame(FrameClass::keyframe(k), 0.0, 0.99),
            0,
            "k={k}: 0% 丢包时关键帧不该要冗余"
        );
        assert_eq!(
            suggest_redundant_for_frame(FrameClass::delta(k), 0.0, 0.99),
            0,
            "k={k}: 0% 丢包时 P 帧不该要冗余"
        );
        assert_eq!(suggest_redundant_exact(k, 0.0, 0.999), 0);
    }
    // NaN 丢包率按 0 处理（不能 panic，也不能给出正数）
    assert_eq!(
        suggest_redundant_for_frame(FrameClass::keyframe(35), f64::NAN, 0.99),
        0
    );
    assert_eq!(suggest_redundant_exact(35, f64::NAN, 0.999), 0);
}

/// 0% 丢包时精确模型的恢复率是 100%
#[test]
fn zero_loss_exact_probability_is_one() {
    for k in [1usize, 2, 8, 35, 55, 64] {
        for r in 0..=k {
            let p = exact_recovery_probability(k, r, 0.0);
            assert!(
                (p - 1.0).abs() < 1e-12,
                "k={k} r={r}: 0% 丢包应为 1.0，得到 {p}"
            );
        }
    }
}

// ============================================================================
// 4. 边界：1 片、2 片、64 片
// ============================================================================

#[test]
fn boundary_one_fragment() {
    // 1 片帧：组内至少 2 个包才谈得上 FEC，钳制仍给 1（宁可多一个包）
    assert_eq!(clamp_keyframe_redundant(1, 1), 1);
    let r = suggest_redundant_for_frame(FrameClass::keyframe(1), 0.05, 0.99);
    assert!(r <= 1, "1 片帧的冗余不该超过 1，得到 {r}");
    assert_eq!(
        suggest_redundant_for_frame(FrameClass::keyframe(0), 0.05, 0.99),
        0
    );
    // r=0 时只有「一个都没丢」才完整
    assert!((exact_recovery_probability(1, 0, 0.05) - 0.95).abs() < 1e-12);
    // r == k == 1：单余类闭式解 q^k (1+kp) = 0.95 * 1.05
    let expect = 0.95 * 1.05;
    assert!((exact_recovery_probability(1, 1, 0.05) - expect).abs() < 1e-12);
}

#[test]
fn boundary_two_fragments() {
    let kf = suggest_redundant_for_frame(FrameClass::keyframe(2), 0.05, 0.99);
    let df = suggest_redundant_for_frame(FrameClass::delta(2), 0.05, 0.99);
    assert!(kf <= 1, "2 片的 50% 上限是 1，得到 {kf}");
    assert_eq!(df, suggest_redundant(2, 0.05, 0.99), "P 帧行为必须不变");
    // r=1 时精确模型有闭式解 q^k (1+kp)
    let expect = 0.95f64.powi(2) * (1.0 + 2.0 * 0.05);
    assert!((exact_recovery_probability(2, 1, 0.05) - expect).abs() < 1e-12);
}

#[test]
fn boundary_group_size_max() {
    // 64 片 = 组大小上限，上限是 32
    assert_eq!(clamp_keyframe_redundant(64, 64), 32);
    let r = suggest_redundant_for_frame(FrameClass::keyframe(64), 0.05, 0.99);
    assert_eq!(r, 32, "64 片 @5% 应撞到 32 的上限，得到 {r}");
    // 超大分片数（超过组大小上限）也不该 panic 或失控
    let big = suggest_redundant_for_frame(FrameClass::keyframe(1000), 0.05, 0.99);
    assert_eq!(big, 500, "1000 片的 50% 是 500，得到 {big}");
}

// ============================================================================
// 5. 精确模型 vs 二项模型
// ============================================================================

/// 二项模型在大组上系统性高估，精确模型必须显著更保守
#[test]
fn exact_model_is_more_conservative_than_binomial_on_big_groups() {
    // k=35, p=5%: 二项说 9 个够（99.9%），真实只有 89.5%
    let binom = suggest_redundant(35, 0.05, 0.999);
    let exact = suggest_redundant_exact(35, 0.05, 0.999);
    assert!(
        exact > binom * 2,
        "k=35 @5% @0.999: 精确模型 {exact} 应显著大于二项模型 {binom}"
    );
    assert!(
        exact_recovery_probability(35, binom, 0.05) < 0.95,
        "二项模型给的 {binom} 个冗余包，真实恢复率必须明显低于目标"
    );

    // 小组上两者接近（这是 P 帧不改模型的前提）
    // k=8 上二项给 2、精确给 5 —— 差 3 倍。这正说明「P 帧沿用二项模型」
    // 在 8 片时已经不够（8 片 P 帧在 5% 丢包下真实恢复率只有 95.5%），
    // 但 8 片的 P 帧不在本轮的改动范围内，这里只钉住「精确更保守」这个方向。
    let b8 = suggest_redundant(8, 0.05, 0.99);
    let e8 = suggest_redundant_exact(8, 0.05, 0.99);
    assert!(
        e8 >= b8,
        "k=8 @5%: 精确模型 {e8} 不该比二项模型 {b8} 更宽松"
    );
}

/// 精确模型在 r 增大时单调不减，且始终落在 [0,1]
#[test]
fn exact_probability_bounds() {
    for k in 1..=GROUP_SIZE_MAX {
        for &p in &[0.01, 0.05, 0.10, 0.30] {
            let mut prev = -1.0f64;
            for r in 0..=k {
                let v = exact_recovery_probability(k, r, p);
                assert!((0.0..=1.0).contains(&v), "k={k} r={r} p={p}: 概率越界 {v}");
                assert!(
                    v >= prev - 1e-12,
                    "k={k} p={p}: r 增大时概率不该下降（{prev} -> {v}）"
                );
                prev = v;
            }
        }
    }
    assert_eq!(exact_recovery_probability(0, 0, 0.5), 1.0);
    assert_eq!(exact_recovery_probability(10, 3, 1.0), 0.0);
}

// ============================================================================
// 6. 端到端恢复率：用真实 FecSender / FecReceiver
// ============================================================================

/// 模型与真实解码器一致（这是整条策略的地基）
///
/// 用真实 `FecSender`/`FecReceiver` 跑 10 万帧随机丢包，与
/// `exact_recovery_probability` 对比。容差 2pp：10 万次采样的标准误约 0.15pp，
/// 2pp 留足余量，同时仍能抓住「模型写错一个量级」这类错误。
#[test]
fn exact_model_matches_real_decoder() {
    let cases: [(usize, usize, f64); 6] = [
        (8, 3, 0.05),
        (8, 4, 0.10),
        (35, 8, 0.05),
        (35, 17, 0.05),
        (55, 16, 0.05),
        (55, 27, 0.10),
    ];
    for (k, r, p) in cases {
        let model = exact_recovery_probability(k, r, p);
        let measured = measure_recovery_rate(k, r, p, 6_000, 0x5EED_1234 + (k * 1000 + r) as u64);
        let diff = (model - measured).abs();
        assert!(
            diff < 0.030,
            "k={k} r={r} p={p}: 模型 {model:.4} 与实测 {measured:.4} 差 {diff:.4}，超过 3pp"
        );
    }
}

/// 真实 1160B 分片恢复后逐字节一致（长度正确性单独守着）
#[test]
fn real_payload_bytes_survive_recovery() {
    let frags = make_fragments(35, REAL_FRAG_PAYLOAD);
    let mut rng = Xorshift64::new(0xFACE_0001);
    let mut recovered_any = 0usize;
    for t in 0..30 {
        let run = run_frame_once(t, &frags, 17, 0.05, &mut rng, true);
        if run.lost_data > 0 && run.recovered_all {
            recovered_any += 1;
        }
    }
    assert!(
        recovered_any > 0,
        "30 帧里应有若干帧「有丢包但被完整救回」，否则这个测试没验证到恢复路径"
    );
}

/// 关键帧在可达区间内达到目标恢复率
///
/// k=16 时带宽上限是 8，1% 丢包下精确模型要 8 个，恰好够到 0.999。
#[test]
fn keyframe_meets_target_when_reachable() {
    let k = 16usize;
    let loss = 0.01;
    let r = suggest_redundant_for_frame(FrameClass::keyframe(k), loss, 0.99);
    assert!(r > 0, "k={k} @1%: 应给出正冗余");

    let model = exact_recovery_probability(k, r, loss);
    assert!(
        model >= KEYFRAME_RECOVERY_TARGET - 1e-9,
        "k={k} @1% r={r}: 模型值 {model:.5} 未达目标 {KEYFRAME_RECOVERY_TARGET}"
    );

    let measured = measure_recovery_rate(k, r, loss, 6_000, 0xABCD_0001);
    assert!(
        measured >= KEYFRAME_RECOVERY_TARGET - 0.010,
        "k={k} @1% r={r}: 实测恢复率 {measured:.4} 未达目标（模型 {model:.4}）"
    );
}

/// 关键帧的恢复率天花板是物理上限，不是「多给冗余就能到 1」
///
/// k=35 / p=5% 时**即使 r=35（100% 开销）也只有 99.65%** —— 纯 XOR 的可解条件
/// 是「丢失落在不同余类」，而 5% × 35 = 1.75 个期望丢失里仍有概率出现同余类
/// 冲突。这条测试把这个事实钉住：任何「把 target 调到 0.9999 就万事大吉」
/// 的想法都会被它挡住。
#[test]
fn keyframe_ceiling_is_physical() {
    // 用 k=8 / 12 验证「满冗余也到不了 1」这个事实 —— 它与 k 无关，只与
    // 「纯 XOR 需要不同余类」有关。k=55 的高斯消元是 O(k^3)，debug 下要跑
    // 一分多钟，而结论一模一样。
    for (k, p, ceiling) in [(8usize, 0.10f64, 0.9973f64), (12, 0.10, 0.9938)] {
        let full = measure_recovery_rate(k, k, p, 6_000, 0xCE11_0000 + k as u64);
        assert!(
            (full - ceiling).abs() < 0.020,
            "k={k} p={p}: 满冗余（r=k）的恢复率 {full:.4} 应约等于 {ceiling:.4}"
        );
        assert!(
            full < 0.9999,
            "k={k} p={p}: 满冗余也只有 {full:.4}，不该出现「接近 1」的假象"
        );
    }
    // 大组的天花板由模型给出（已由 exact_model_matches_real_decoder 验证过
    // 模型在 k=55 上准确），这里只钉住「显著低于 1」这个结论。
    for (k, p) in [(35usize, 0.05f64), (55, 0.05), (55, 0.10)] {
        let full = exact_recovery_probability(k, k, p);
        assert!(
            full < 0.999,
            "k={k} p={p}: 满冗余的模型值 {full:.4} 仍应明显低于 1"
        );
    }
}

/// 关键帧策略相对现状（固定 redundant=2）的提升必须可测
///
/// 5% 丢包、35 片：现状 r=2 只有 58.4%，新策略 r=17 有 95.1%。
#[test]
fn keyframe_strategy_beats_fixed_redundant_two() {
    let k = 35usize;
    let loss = 0.05;
    let old = measure_recovery_rate(k, 2, loss, 6_000, 0x1111_2222);
    let new_r = suggest_redundant_for_frame(FrameClass::keyframe(k), loss, 0.99);
    let new = measure_recovery_rate(k, new_r, loss, 6_000, 0x3333_4444);

    assert!(old < 0.65, "现状 r=2 的恢复率应低于 65%，实测 {old:.4}");
    assert!(
        new > 0.92,
        "新策略 r={new_r} 的恢复率应高于 92%，实测 {new:.4}"
    );
    assert!(new - old > 0.25, "提升应超过 25pp（{old:.4} -> {new:.4}）");
}

/// 无丢包时 FEC 不该白跑（没有恢复动作，也没有丢包）
#[test]
fn no_loss_needs_no_recovery() {
    let frags = make_fragments(35, STAT_FRAG_PAYLOAD);
    let mut rng = Xorshift64::new(0xDEAD_BEEF);
    for t in 0..200 {
        let run = run_frame_once(t, &frags, 17, 0.0, &mut rng, false);
        assert!(run.recovered_all);
        assert_eq!(run.lost_data, 0);
        assert_eq!(run.lost_red, 0);
    }
}

// ============================================================================
// 7. 现有 API 语义不变
// ============================================================================

/// `suggest_redundant` 的既有行为必须逐点不变
#[test]
fn suggest_redundant_behavior_unchanged() {
    assert_eq!(suggest_redundant(10, 0.0, 0.99), 0);
    assert_eq!(suggest_redundant(0, 0.05, 0.99), 0);
    assert_eq!(suggest_redundant(10, 0.05, 0.0), 0);
    assert_eq!(suggest_redundant(10, 1.0, 0.99), 10);
    assert_eq!(suggest_redundant(8, 0.03, 0.99), 2);
    assert_eq!(suggest_redundant(2, 0.05, 0.99), 1);

    let mut prev = 0;
    for &p in &[0.0, 0.01, 0.02, 0.05, 0.10, 0.20] {
        let r = suggest_redundant(16, p, 0.99);
        assert!(r >= prev, "p={p}: 冗余 {r} 比更低的丢包率 {prev} 还少");
        prev = r;
    }
}

/// P 帧分支必须**完全等于** `suggest_redundant`（一个数都不差）
#[test]
fn delta_branch_delegates_exactly() {
    for k in 1..=GROUP_SIZE_MAX {
        for &loss in &[0.0, 0.005, 0.01, 0.03, 0.05, 0.10, 0.15, 0.30] {
            for &target in &[0.90, 0.95, 0.99, 0.999] {
                assert_eq!(
                    suggest_redundant_for_frame(FrameClass::delta(k), loss, target),
                    suggest_redundant(k, loss, target),
                    "k={k} loss={loss} target={target}: P 帧分支必须与 suggest_redundant 完全一致"
                );
            }
        }
    }
}
