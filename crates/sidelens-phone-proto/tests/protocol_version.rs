//! 发布层与设备链路层的协议版本一致性。
//!
//! `sidelens-update::PROTOCOL_VERSION` 是发布决策用的副本（为了让发布层
//! 不依赖媒体协议栈），真正的定义在 `sidelens-phone-proto::VERSION`。
//! 两份值漂移会导致一个很隐蔽的 bug：电脑端按清单里的 `min_supported_protocol`
//! 判定手机端"协议太老、拒绝会话"，而手机端实际发的是新协议 ——
//! 现象是「手机明明是最新版，电脑端却一直说协议不兼容」。
//!
//! 这个测试就是防它的唯一手段。

#[test]
fn 发布层的协议版本与手机链路协议一致() {
    assert_eq!(
        sidelens_update::PROTOCOL_VERSION,
        u32::from(sidelens_phone_proto::VERSION),
        "sidelens-update::PROTOCOL_VERSION 与 sidelens-phone-proto::VERSION 漂移了"
    );
}

#[test]
fn 发布层语义版本与_workspace_版本一致() {
    // 两个 crate 都用 version.workspace = true，因此这一条本质上断言
    // 打包脚本读到的版本就是运行期报告的版本。
    assert_eq!(sidelens_update::VERSION, env!("CARGO_PKG_VERSION"));
    assert!(sidelens_update::Version::parse(sidelens_update::VERSION).is_ok());
}
