#![cfg(windows)]
//
// 整个测试文件都是 Windows 专属的：要 CoInitializeEx、要 Media Foundation、
// 要真实注册表。非 Windows target 上直接跳过，否则 `cargo test` 会因为
// 「找不到 windows crate」而失败（见 docs/cross-platform.md P2）。

//! IMFMediaStream 状态机测试
//!
//! 验证 MF 契约：流创建后是 STOPPED，只有 RUNNING 才接受 RequestSample。

use sidelens_vcam::stream::{SideLensMediaStream, StreamState, VIDEO_STREAM_ID};

#[test]
fn stream_starts_stopped() {
    let s = StreamState::new(VIDEO_STREAM_ID);
    // MF 契约：流创建后必须显式 SetStreamState(RUNNING)
    assert_eq!(
        s.state(),
        windows::Win32::Media::MediaFoundation::MF_STREAM_STATE_STOPPED
    );
}

#[test]
fn set_running_transitions() {
    let s = StreamState::new(VIDEO_STREAM_ID);
    s.set_running();
    assert_eq!(
        s.state(),
        windows::Win32::Media::MediaFoundation::MF_STREAM_STATE_RUNNING
    );
}

#[test]
fn set_stopped_transitions_back() {
    let s = StreamState::new(VIDEO_STREAM_ID);
    s.set_running();
    s.set_stopped();
    assert_eq!(
        s.state(),
        windows::Win32::Media::MediaFoundation::MF_STREAM_STATE_STOPPED
    );
}

#[test]
fn stream_id_is_preserved() {
    let s = StreamState::new(42);
    assert_eq!(s.stream_id(), 42);
}

#[test]
fn event_queue_is_created() {
    let s = StreamState::new(VIDEO_STREAM_ID);
    // MFCreateEventQueue 应该成功
    assert!(s.queue().is_ok(), "事件队列应创建成功");
}

#[test]
fn descriptor_is_initially_absent() {
    let s = StreamState::new(VIDEO_STREAM_ID);
    // 还没绑定描述符时应返回错误而不是 panic
    assert!(s.descriptor().is_err());
}

#[test]
fn pending_requests_start_empty() {
    let s = StreamState::new(VIDEO_STREAM_ID);
    assert_eq!(s.pending_count(), 0);
}

#[test]
fn stats_start_at_zero() {
    let s = StreamState::new(VIDEO_STREAM_ID);
    let snap = s.stats.snapshot();
    assert_eq!(snap.samples_requested, 0);
    assert_eq!(snap.samples_delivered, 0);
    assert_eq!(snap.rejected_requests, 0);
}

#[test]
fn media_stream_can_be_created() {
    // 只验证能构造，不验证 COM 生命周期
    let s = SideLensMediaStream::new(VIDEO_STREAM_ID);
    assert_eq!(s.state().stream_id(), VIDEO_STREAM_ID);
}
