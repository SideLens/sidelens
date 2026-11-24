//! XOR 奇偶校验 FEC 测试
//!
//! 核心验证点：
//!
//! 1. 无丢包时不产生任何恢复（不能把"没丢"误判成"丢了又救回来"）
//! 2. 单丢一定能恢复，且内容逐字节一致
//! 3. 丢包数超过冗余数时**不返回任何东西**（宁可交给 NACK，也不返回半个解）
//! 4. 同余类双丢是无解的 —— 这是本方案的已知缺陷，用测试把它钉死，
//!    避免以后有人"优化"成逐包套公式后把错误数据当恢复结果吐出去
//! 5. **任何情况下都不返回错误数据**：解不出来的未知数跳过，解得出的一定逐字节正确。
//!    两条 `exhaustive_*` 用例把 8 个包的全部丢包组合穷举一遍来守这条不变量，
//!    同时机器校验模块文档里那张"能力表"（单丢全恢复、redundant=2 双丢只救 16/28）。

use sidelens_core::fec::{FecConfig, FecDecoder, FecEncoder};

/// 测试统一用的配置：每组 10 个原始包 + 2 个冗余包（20% 开销）
fn cfg() -> FecConfig {
    FecConfig {
        group_size: 10,
        redundant: 2,
        max_packet_size: 1200,
    }
}

/// 造一组长度各不相同的确定性载荷（第 i 个包长度 20 + 7i）
fn packets(n: usize) -> Vec<Vec<u8>> {
    (0..n)
        .map(|i| {
            let len = 20 + i * 7;
            (0..len).map(|j| ((i * 31 + j * 7) % 256) as u8).collect()
        })
        .collect()
}

/// 组内最长包的字节数
fn max_len(pkts: &[Vec<u8>]) -> usize {
    pkts.iter().map(|p| p.len()).max().unwrap_or(0)
}

/// 把一组原始包喂进编码器并取出冗余包
fn encode(pkts: &[Vec<u8>]) -> Vec<Vec<u8>> {
    let mut enc = FecEncoder::new(cfg()).expect("配置应合法");
    for p in pkts {
        enc.push(p);
    }
    assert!(enc.is_full(), "喂满 group_size 个包后应当已满");
    enc.take_redundant()
}

/// 把"收到的那部分包"喂进解码器
///
/// `dropped` 里的下标模拟丢包：这些下标不给原始包，但**所有冗余包照常投递**
/// （冗余包本身也可能丢，那是另一回事，这里只测"数据包丢、冗余包全到"的场景）。
fn decode(pkts: &[Vec<u8>], redundants: &[Vec<u8>], dropped: &[usize]) -> FecDecoder {
    let mut dec = FecDecoder::new(cfg()).expect("配置应合法");
    for (i, p) in pkts.iter().enumerate() {
        if !dropped.contains(&i) {
            dec.push_original(i, p);
        }
    }
    for (k, r) in redundants.iter().enumerate() {
        dec.push_redundant(k, r);
    }
    dec
}

/// 断言恢复结果与原始包逐字节相等（允许尾部补 0 到补齐长度）
fn assert_payload_eq(recovered: &[u8], original: &[u8], ctx: &str) {
    assert!(
        recovered.len() >= original.len(),
        "{}：恢复长度 {} 不应短于原始长度 {}",
        ctx,
        recovered.len(),
        original.len()
    );
    assert_eq!(
        &recovered[..original.len()],
        original,
        "{}：恢复出的前 {} 字节必须与原始包完全一致",
        ctx,
        original.len()
    );
    assert!(
        recovered[original.len()..].iter().all(|&b| b == 0),
        "{}：补齐区必须全是 0，实际为 {:?}",
        ctx,
        &recovered[original.len()..]
    );
}

/// 在恢复结果里按下标找载荷
fn find(recovered: &[(usize, Vec<u8>)], index: usize) -> Option<&[u8]> {
    recovered
        .iter()
        .find(|(i, _)| *i == index)
        .map(|(_, d)| d.as_slice())
}

// ---------------------------------------------------------------- 基础行为

#[test]
fn no_loss_recovers_nothing() {
    let pkts = packets(10);
    let red = encode(&pkts);
    let mut dec = decode(&pkts, &red, &[]);

    assert_eq!(dec.received(), 10, "10 个原始包应当全部收到");
    assert_eq!(dec.missing(), 0, "无丢包时缺失数应为 0");
    assert!(
        dec.recover().is_empty(),
        "无丢包时不应恢复任何包（否则统计会把「没丢」算成「救回」）"
    );
    assert_eq!(dec.recovered_count(), 0, "恢复计数应保持 0");
}

#[test]
fn single_loss_is_recovered_exactly() {
    let pkts = packets(10);
    let red = encode(&pkts);
    let mut dec = decode(&pkts, &red, &[4]);

    assert_eq!(dec.missing(), 1, "丢了 1 个包，缺失数应为 1");
    let recovered = dec.recover();
    assert_eq!(recovered.len(), 1, "单丢应当恢复出 1 个包");
    assert_eq!(recovered[0].0, 4, "恢复的应当是下标 4 的包");
    assert_payload_eq(&recovered[0].1, &pkts[4], "单丢恢复");

    assert_eq!(dec.missing(), 0, "恢复成功后不应再缺包");
    assert_eq!(dec.recovered_count(), 1, "恢复计数应为 1");
    assert!(dec.recover().is_empty(), "再次调用不应重复返回（幂等）");
}

#[test]
fn two_losses_in_different_classes_are_recovered() {
    let pkts = packets(10);
    let red = encode(&pkts);
    // 下标 0 属于余类 0，下标 1 属于余类 1：两个不同的余类，方程组可解
    let mut dec = decode(&pkts, &red, &[0, 1]);

    let recovered = dec.recover();
    assert_eq!(recovered.len(), 2, "不同余类的双丢应当都能恢复");
    assert_payload_eq(
        find(&recovered, 0).expect("应当恢复出下标 0"),
        &pkts[0],
        "双丢-下标 0",
    );
    assert_payload_eq(
        find(&recovered, 1).expect("应当恢复出下标 1"),
        &pkts[1],
        "双丢-下标 1",
    );
    assert_eq!(dec.missing(), 0, "双丢恢复后不应再缺包");
}

#[test]
fn three_losses_exceed_redundancy_and_are_rejected() {
    let pkts = packets(10);
    let red = encode(&pkts);
    let mut dec = decode(&pkts, &red, &[0, 1, 2]);

    assert_eq!(dec.missing(), 3, "丢了 3 个包");
    assert!(
        dec.recover().is_empty(),
        "丢包数超过冗余数时不应返回任何恢复结果"
    );
    assert_eq!(dec.recovered_count(), 0, "失败时不应计入恢复统计");
    assert_eq!(dec.missing(), 3, "失败时不应改动已收到状态");
}

#[test]
fn same_class_double_loss_is_unrecoverable() {
    // 已知缺陷：两个丢失包落在同一余类时，那个余类的冗余包同时排除了两者，
    // 方程组欠定。这里必须返回空，而不是吐出一个 P_0 XOR P_2 的错误结果。
    let pkts = packets(10);
    let red = encode(&pkts);
    let mut dec = decode(&pkts, &red, &[0, 2]);

    assert!(
        dec.recover().is_empty(),
        "同余类双丢无解，必须返回空而不是错误数据"
    );
    assert_eq!(dec.missing(), 2, "无解时缺失数保持 2");
}

#[test]
fn partial_recovery_returns_only_determined_packets() {
    // 部分恢复：丢 3 个、redundant = 3。如果其中一个包独占一个余类，
    // 那一个能解出来，另外两个同余类的解不出。必须"能救的救、救不了的留给 NACK"，
    // 而不是因为整组解不全就一个都不给。
    let config = FecConfig {
        group_size: 10,
        redundant: 3,
        max_packet_size: 1200,
    };
    let pkts = packets(10);

    let mut enc = FecEncoder::new(config).expect("配置应合法");
    for p in &pkts {
        enc.push(p);
    }
    let red = enc.take_redundant();

    // 下标 0 和 3 同属余类 0（3 % 3 == 0），下标 1 独占余类 1
    let dropped = [0usize, 1, 3];
    let mut dec = FecDecoder::new(config).expect("配置应合法");
    for (i, p) in pkts.iter().enumerate() {
        if !dropped.contains(&i) {
            dec.push_original(i, p);
        }
    }
    for (k, r) in red.iter().enumerate() {
        dec.push_redundant(k, r);
    }

    let recovered = dec.recover();
    assert_eq!(
        recovered.len(),
        1,
        "只应当恢复出独占余类的那个包，实际恢复 {:?}",
        recovered.iter().map(|(i, _)| *i).collect::<Vec<_>>()
    );
    assert_eq!(recovered[0].0, 1, "能解出来的应当是下标 1");
    assert_payload_eq(&recovered[0].1, &pkts[1], "部分恢复");
    assert_eq!(dec.missing(), 2, "剩下 2 个仍需 NACK");
}

#[test]
fn four_redundant_recovers_triple_loss_across_classes() {
    // redundant = 4、丢 3 个且分属三个余类：三行方程线性无关，全部可解。
    // 这条用例说明"冗余数 >= 缺失数"本身不保证可解，还要看余类分布。
    let config = FecConfig {
        group_size: 10,
        redundant: 4,
        max_packet_size: 1200,
    };
    let pkts = packets(10);

    let mut enc = FecEncoder::new(config).expect("配置应合法");
    for p in &pkts {
        enc.push(p);
    }
    let red = enc.take_redundant();
    assert_eq!(red.len(), 4, "应当产出 4 个冗余包");

    // 0、1、2 分属余类 0、1、2
    let dropped = [0usize, 1, 2];
    let mut dec = FecDecoder::new(config).expect("配置应合法");
    for (i, p) in pkts.iter().enumerate() {
        if !dropped.contains(&i) {
            dec.push_original(i, p);
        }
    }
    for (k, r) in red.iter().enumerate() {
        dec.push_redundant(k, r);
    }

    let recovered = dec.recover();
    assert_eq!(recovered.len(), 3, "三个不同余类的缺失应当全部恢复");
    for idx in dropped {
        assert_payload_eq(
            find(&recovered, idx).expect("应当恢复出该下标"),
            &pkts[idx],
            &format!("三丢恢复-下标 {}", idx),
        );
    }
    assert_eq!(dec.missing(), 0, "全部恢复后不应再缺包");
}

#[test]
fn all_single_loss_positions_recover_byte_exactly() {
    let pkts = packets(10);
    let red = encode(&pkts);

    for dropped in 0..10usize {
        let mut dec = decode(&pkts, &red, &[dropped]);
        let recovered = dec.recover();
        assert_eq!(recovered.len(), 1, "丢下标 {} 时应当恢复出 1 个包", dropped);
        assert_eq!(recovered[0].0, dropped, "恢复的下标应当与丢失下标一致");
        assert_payload_eq(
            &recovered[0].1,
            &pkts[dropped],
            &format!("单丢下标 {}", dropped),
        );
    }
}

#[test]
fn exhaustive_recovery_never_returns_wrong_bytes() {
    // 穷举验证：任意丢包组合下，返回的每一个恢复包都必须逐字节正确。
    // 这是本模块最重要的一条不变量 —— 宁可不恢复，也绝不把 XOR 出来的
    // 中间结果（比如 P_0 XOR P_1）当成某个包的正文交出去。
    //
    // 同时在 group_size = 8 上复核模块文档里的能力表：
    // redundant = 2 时双丢 16/28 可全恢复，redundant = 3 时任意三丢都解不出。
    let config = FecConfig {
        group_size: 8,
        redundant: 2,
        max_packet_size: 1200,
    };
    let pkts = packets(8);
    let maxlen = max_len(&pkts);

    let mut enc = FecEncoder::new(config).expect("配置应合法");
    for p in &pkts {
        enc.push(p);
    }
    let red = enc.take_redundant();

    let mut single_full = 0;
    let mut double_full = 0;
    let mut double_total = 0;

    for mask in 0u32..(1u32 << 8) {
        let dropped: Vec<usize> = (0..8).filter(|i| mask & (1u32 << i) != 0).collect();
        if dropped.is_empty() || dropped.len() > 2 {
            continue;
        }

        let mut dec = FecDecoder::new(config).expect("配置应合法");
        for (i, p) in pkts.iter().enumerate() {
            if !dropped.contains(&i) {
                dec.push_original(i, p);
            }
        }
        for (k, r) in red.iter().enumerate() {
            dec.push_redundant(k, r);
        }

        let recovered = dec.recover();
        for (idx, data) in &recovered {
            assert!(
                dropped.contains(idx),
                "不该恢复出没有丢失的下标 {}（丢弃集合 {:?}）",
                idx,
                dropped
            );
            assert_eq!(data.len(), maxlen, "恢复出的包长度应为组内最长包");
            assert_payload_eq(data, &pkts[*idx], &format!("穷举-丢弃 {:?}", dropped));
        }

        if dropped.len() == 1 {
            assert_eq!(recovered.len(), 1, "单丢必须恢复，丢弃 {:?}", dropped);
            single_full += 1;
        } else {
            double_total += 1;
            if recovered.len() == 2 {
                double_full += 1;
            }
        }
    }

    assert_eq!(single_full, 8, "8 种单丢应当全部恢复");
    assert_eq!(double_total, 28, "8 个包的双丢组合应为 28 种");
    assert_eq!(
        double_full, 16,
        "redundant = 2 时只有不同余类的 16 种双丢能全恢复，实际 {}",
        double_full
    );
}

#[test]
fn exhaustive_three_loss_with_three_redundant_never_fully_recovers() {
    // 模块文档里"redundant = 3 时任意三丢都无解"这条结论的机器验证。
    // 注意仍然可能有部分恢复（某个包独占一个余类），所以只断言"救不全"。
    let config = FecConfig {
        group_size: 8,
        redundant: 3,
        max_packet_size: 1200,
    };
    let pkts = packets(8);

    let mut enc = FecEncoder::new(config).expect("配置应合法");
    for p in &pkts {
        enc.push(p);
    }
    let red = enc.take_redundant();

    let mut triples = 0;
    for mask in 0u32..(1u32 << 8) {
        let dropped: Vec<usize> = (0..8).filter(|i| mask & (1u32 << i) != 0).collect();
        if dropped.len() != 3 {
            continue;
        }
        triples += 1;

        let mut dec = FecDecoder::new(config).expect("配置应合法");
        for (i, p) in pkts.iter().enumerate() {
            if !dropped.contains(&i) {
                dec.push_original(i, p);
            }
        }
        for (k, r) in red.iter().enumerate() {
            dec.push_redundant(k, r);
        }

        let recovered = dec.recover();
        assert!(
            recovered.len() < 3,
            "redundant = 3 时三丢不该全部恢复，丢弃 {:?} 却救回了 {:?}",
            dropped,
            recovered.iter().map(|(i, _)| *i).collect::<Vec<_>>()
        );
        for (idx, data) in &recovered {
            assert_payload_eq(data, &pkts[*idx], "部分恢复也必须逐字节正确");
        }
    }

    assert_eq!(triples, 56, "8 个包的三丢组合应为 56 种");
}

#[test]
fn variable_length_packets_recover_correctly() {
    let pkts = packets(10);
    // 确认这组包确实是变长的
    assert_ne!(pkts[0].len(), pkts[9].len(), "测试数据应当是变长包");
    let red = encode(&pkts);

    // 丢最长的那个（下标 9），它的长度与补齐长度一致，不需要截断
    let mut dec = decode(&pkts, &red, &[9]);
    let recovered = dec.recover();
    assert_eq!(recovered.len(), 1);
    assert_eq!(
        recovered[0].1.len(),
        max_len(&pkts),
        "恢复出的包长度应等于组内最长包长度"
    );
    assert_eq!(recovered[0].1, pkts[9], "最长包恢复后应逐字节相等");

    // 丢最短的那个（下标 0），恢复出来会带尾部补 0，前缀必须正确
    let mut dec = decode(&pkts, &red, &[0]);
    let recovered = dec.recover();
    assert_payload_eq(&recovered[0].1, &pkts[0], "变长包-最短");
    assert_eq!(
        recovered[0].1.len(),
        max_len(&pkts),
        "短包恢复后长度会被补齐到组内最长包"
    );
}

#[test]
fn empty_packets_are_handled() {
    // 空包合法：收尾时可以用它把半组补齐，让尾组也被保护
    let mut pkts = packets(10);
    pkts[0] = Vec::new();
    pkts[5] = Vec::new();
    let red = encode(&pkts);

    // 丢一个空包：恢复出来的应当是全 0，长度等于组内最长包
    let mut dec = decode(&pkts, &red, &[0]);
    let recovered = dec.recover();
    assert_eq!(recovered.len(), 1, "空包丢了也应当能恢复");
    assert_eq!(
        recovered[0].1.len(),
        max_len(&pkts),
        "空包恢复后长度等于组内最长包"
    );
    assert!(
        recovered[0].1.iter().all(|&b| b == 0),
        "空包恢复出来必须是全 0"
    );

    // 丢一个非空包，空包作为已收到的成员参与 XOR，不能影响结果
    let mut dec = decode(&pkts, &red, &[7]);
    let recovered = dec.recover();
    assert_payload_eq(&recovered[0].1, &pkts[7], "组内含空包时恢复");
}

#[test]
fn all_empty_group_produces_zero_length_redundants() {
    let pkts = vec![Vec::new(); 10];
    let red = encode(&pkts);
    assert_eq!(red.len(), 2, "仍然应当产出 2 个冗余包");
    for r in &red {
        assert!(r.is_empty(), "整组都是空包时冗余包长度应为 0");
    }

    // 全空组丢一个包也能恢复成空包
    let mut dec = decode(&pkts, &red, &[3]);
    let recovered = dec.recover();
    assert_eq!(recovered.len(), 1);
    assert!(recovered[0].1.is_empty(), "恢复出来的空包长度应为 0");
}

// ---------------------------------------------------------------- 配置

#[test]
fn invalid_configs_are_rejected() {
    // group_size 为 0
    let bad = FecConfig {
        group_size: 0,
        redundant: 1,
        max_packet_size: 1200,
    };
    assert!(bad.validate().is_err(), "group_size 为 0 应当被拒绝");
    assert!(FecEncoder::new(bad).is_err(), "编码器应拒绝非法配置");
    assert!(FecDecoder::new(bad).is_err(), "解码器应拒绝非法配置");

    // redundant 为 0
    let bad = FecConfig {
        group_size: 10,
        redundant: 0,
        max_packet_size: 1200,
    };
    assert!(bad.validate().is_err(), "redundant 为 0 应当被拒绝");
    assert!(FecEncoder::new(bad).is_err());

    // redundant 大于 group_size
    let bad = FecConfig {
        group_size: 4,
        redundant: 5,
        max_packet_size: 1200,
    };
    assert!(
        bad.validate().is_err(),
        "redundant 大于 group_size 应当被拒绝"
    );
    assert!(FecDecoder::new(bad).is_err());

    // max_packet_size 为 0
    let bad = FecConfig {
        group_size: 10,
        redundant: 2,
        max_packet_size: 0,
    };
    assert!(bad.validate().is_err(), "max_packet_size 为 0 应当被拒绝");
    assert!(FecEncoder::new(bad).is_err());

    // 合法配置不应被拒绝
    assert!(cfg().validate().is_ok(), "默认配置应当合法");
}

#[test]
fn overhead_ratio_is_computed_correctly() {
    assert!(
        (FecConfig::default().overhead_ratio() - 0.2).abs() < 1e-9,
        "默认配置（10/2）的冗余占比应为 0.2"
    );

    let half = FecConfig {
        group_size: 10,
        redundant: 5,
        max_packet_size: 1200,
    };
    assert!(
        (half.overhead_ratio() - 0.5).abs() < 1e-9,
        "10/5 的冗余占比应为 0.5"
    );

    let single = FecConfig {
        group_size: 4,
        redundant: 1,
        max_packet_size: 1200,
    };
    assert!(
        (single.overhead_ratio() - 0.25).abs() < 1e-9,
        "4/1 的冗余占比应为 0.25"
    );

    // group_size 为 0 时不 panic，返回 0.0
    let zero = FecConfig {
        group_size: 0,
        redundant: 0,
        max_packet_size: 1,
    };
    assert_eq!(zero.overhead_ratio(), 0.0, "group_size 为 0 时返回 0.0");
    assert_eq!(
        FecConfig::default().redundant_budget(),
        2400,
        "默认配置每组冗余预算应为 2 * 1200"
    );
}

// ---------------------------------------------------------------- 编码器状态机

#[test]
fn take_redundant_returns_configured_count() {
    let pkts = packets(10);
    let mut enc = FecEncoder::new(cfg()).expect("配置应合法");
    for p in &pkts {
        enc.push(p);
    }
    let red = enc.take_redundant();
    assert_eq!(red.len(), 2, "应当返回 redundant 个冗余包");
    assert_eq!(enc.pending(), 0, "取走后当前组应当被清空");
    assert!(!enc.is_full(), "取走后本组不再处于满状态");
}

#[test]
fn redundant_packets_have_max_group_length() {
    let pkts = packets(10);
    let red = encode(&pkts);
    let expect = max_len(&pkts);
    for (k, r) in red.iter().enumerate() {
        assert_eq!(
            r.len(),
            expect,
            "第 {} 个冗余包长度应等于组内最长包 {}",
            k,
            expect
        );
    }
}

#[test]
fn partial_group_yields_no_redundant() {
    let pkts = packets(10);
    let mut enc = FecEncoder::new(cfg()).expect("配置应合法");
    for p in &pkts[..7] {
        assert!(!enc.push(p), "前 7 个包不应让本组变满");
    }
    assert_eq!(enc.pending(), 7, "当前组应有 7 个包");
    assert!(enc.take_redundant().is_empty(), "未满时不应产出冗余包");
    assert_eq!(enc.pending(), 7, "未满时取冗余包不应清空当前组");

    // 补满后仍能正常产出
    for p in &pkts[7..] {
        enc.push(p);
    }
    assert_eq!(enc.take_redundant().len(), 2, "补满后应当正常产出冗余包");
}

#[test]
fn reset_clears_current_group() {
    let pkts = packets(10);
    let mut enc = FecEncoder::new(cfg()).expect("配置应合法");
    for p in &pkts[..6] {
        enc.push(p);
    }
    assert_eq!(enc.pending(), 6);

    enc.reset();
    assert_eq!(enc.pending(), 0, "reset 后当前组应当为空");
    assert!(!enc.is_full(), "reset 后不应处于满状态");
    assert!(enc.take_redundant().is_empty(), "reset 后取冗余包应为空");

    // reset 不影响已完成的组序号
    for p in &pkts {
        enc.push(p);
    }
    assert_eq!(enc.group_index(), 1, "reset 不应回退组序号");
}

#[test]
fn group_index_increments_per_full_group() {
    let pkts = packets(10);
    let mut enc = FecEncoder::new(cfg()).expect("配置应合法");
    assert_eq!(enc.group_index(), 0, "新建时组序号为 0");

    for p in &pkts[..9] {
        enc.push(p);
    }
    assert_eq!(enc.group_index(), 0, "未满时组序号不变");

    assert!(enc.push(&pkts[9]), "第 10 个包应让本组变满");
    assert_eq!(enc.group_index(), 1, "凑满一组后组序号应为 1");

    enc.take_redundant();
    for p in &pkts {
        enc.push(p);
    }
    assert_eq!(enc.group_index(), 2, "再凑满一组后组序号应为 2");
}

// ---------------------------------------------------------------- 解码器状态机

#[test]
fn decoder_counts_received_and_missing() {
    let pkts = packets(10);
    let red = encode(&pkts);
    let mut dec = decode(&pkts, &red, &[2, 5, 8]);

    assert_eq!(dec.received(), 7, "应当收到 7 个原始包");
    assert_eq!(dec.missing(), 3, "应当还缺 3 个");
    assert_eq!(dec.recovered_count(), 0, "还没调用 recover，计数应为 0");

    // 丢了 3 个，超过冗余数 2：即便冗余包全在也救不回来
    assert!(
        dec.recover().is_empty(),
        "缺失数超过冗余数时不应返回任何恢复结果"
    );
    assert_eq!(dec.missing(), 3, "无解时缺失数保持不变");
}

#[test]
fn decoder_ignores_out_of_range_and_duplicate_pushes() {
    let mut dec = FecDecoder::new(cfg()).expect("配置应合法");
    let p = vec![1u8, 2, 3];

    dec.push_original(0, &p);
    // 同一下标再投一次：先到为准，不应覆盖也不应重复计数
    dec.push_original(0, &[9u8, 9, 9]);
    assert_eq!(dec.received(), 1, "重复投递不应增加已收到计数");

    // 越界下标直接忽略，不 panic
    dec.push_original(10, &p);
    dec.push_original(usize::MAX, &p);
    assert_eq!(dec.received(), 1, "越界下标应被忽略");

    // 冗余包下标越界同样忽略
    dec.push_redundant(2, &p);
    assert!(dec.recover().is_empty(), "没有缺失包时不应恢复任何东西");
}

#[test]
fn next_group_starts_from_scratch_but_keeps_stats() {
    let pkts = packets(10);
    let red = encode(&pkts);
    let mut dec = decode(&pkts, &red, &[3]);
    let recovered = dec.recover();
    assert_eq!(recovered.len(), 1);
    assert_eq!(dec.recovered_count(), 1);

    dec.next_group();
    assert_eq!(dec.received(), 0, "切组后已收到数应清零");
    assert_eq!(dec.missing(), 10, "切组后应缺满一整组");
    assert_eq!(dec.recovered_count(), 1, "累计统计不应被切组清零");
    assert!(dec.recover().is_empty(), "新组没有收到任何包，无法恢复");
}

#[test]
fn multiple_groups_work_in_sequence() {
    let pkts_a = packets(10);
    let pkts_b: Vec<Vec<u8>> = packets(10)
        .iter()
        .map(|p| p.iter().map(|b| b.wrapping_add(17)).collect())
        .collect();

    let mut enc = FecEncoder::new(cfg()).expect("配置应合法");
    let mut dec = FecDecoder::new(cfg()).expect("配置应合法");

    // 第一组：丢下标 6
    for p in &pkts_a {
        enc.push(p);
    }
    let red_a = enc.take_redundant();
    for (i, p) in pkts_a.iter().enumerate() {
        if i != 6 {
            dec.push_original(i, p);
        }
    }
    for (k, r) in red_a.iter().enumerate() {
        dec.push_redundant(k, r);
    }
    let recovered = dec.recover();
    assert_eq!(recovered.len(), 1, "第一组应当恢复 1 个包");
    assert_payload_eq(&recovered[0].1, &pkts_a[6], "第一组恢复");
    dec.next_group();

    // 第二组：丢下标 0 和 9（不同余类，可恢复）
    for p in &pkts_b {
        enc.push(p);
    }
    let red_b = enc.take_redundant();
    for (i, p) in pkts_b.iter().enumerate() {
        if i != 0 && i != 9 {
            dec.push_original(i, p);
        }
    }
    for (k, r) in red_b.iter().enumerate() {
        dec.push_redundant(k, r);
    }
    let recovered = dec.recover();
    assert_eq!(recovered.len(), 2, "第二组应当恢复 2 个包");
    assert_payload_eq(
        find(&recovered, 0).expect("应当恢复出下标 0"),
        &pkts_b[0],
        "第二组恢复-下标 0",
    );
    assert_payload_eq(
        find(&recovered, 9).expect("应当恢复出下标 9"),
        &pkts_b[9],
        "第二组恢复-下标 9",
    );

    assert_eq!(dec.recovered_count(), 3, "两组累计恢复 3 个包");
    assert_eq!(enc.group_index(), 2, "编码器应当完成 2 组");
}

#[test]
fn single_redundant_recovers_any_single_loss() {
    // redundant = 1 时退化成经典单奇偶校验：R_0 包含全部原始包
    let config = FecConfig {
        group_size: 8,
        redundant: 1,
        max_packet_size: 1200,
    };
    let pkts = packets(8);

    let mut enc = FecEncoder::new(config).expect("配置应合法");
    for p in &pkts {
        enc.push(p);
    }
    let red = enc.take_redundant();
    assert_eq!(red.len(), 1, "redundant = 1 时只产出 1 个冗余包");
    assert_eq!(red[0].len(), max_len(&pkts), "冗余包长度等于组内最长包");

    for dropped in 0..8usize {
        let mut dec = FecDecoder::new(config).expect("配置应合法");
        for (i, p) in pkts.iter().enumerate() {
            if i != dropped {
                dec.push_original(i, p);
            }
        }
        dec.push_redundant(0, &red[0]);
        let recovered = dec.recover();
        assert_eq!(
            recovered.len(),
            1,
            "redundant = 1 时丢下标 {} 应当能恢复",
            dropped
        );
        assert_payload_eq(
            &recovered[0].1,
            &pkts[dropped],
            &format!("单冗余-单丢下标 {}", dropped),
        );
    }
}
