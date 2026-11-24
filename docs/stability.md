# 稳定性与资源占用

> 长时间运行的数据。每次改动热路径后应重跑。

## 60 秒全功能实测

配置：5% 丢包 + FEC-2 + Opus + H.264 1280x720 + headless 渲染

```text
frames                3556
playable_ms           14.77 / 22.49      p50 / p95
video_decoded         3411
video_decode_failed   0
video_need_keyframe   15              （0.4%，GOP 边界正常损耗）
render_frames         3411              （与 video_decoded 完全一致）
render_failed         0
audio_decoded         2920
audio_decode_failed   0
fec_recovered         23
fec_groups_pruned     3498
video_dropped_late    0
```

## 内存：收敛，不泄漏

| 时刻 | 工作集 | 增量 |
| --- | --- | --- |
| 启动 | 13.1 MB | — |
| 30s | 26.7 MB | +13.6 |
| 60s | 26.9 MB | +0.2 |
| 90s | 26.9 MB | **0** |

30 秒后完全停止增长。这是一次性分配后复用的缓存：

| 缓冲 | 大小 |
| --- | --- |
| 解码器 NV12（1280x720） | 1.38 MB |
| 渲染层 RGBA（1280x720） | 3.7 MB |
| FEC 组状态（保留 8 帧） | ~0.3 MB |
| 抖动缓冲（自适应 target） | 几十帧 x 1.2KB |
| Opus 解码 PCM | 2880 x 2B = 5.7 KB |

若是泄漏，90 秒时应继续线性增长到 40 MB+。实测停在 26.9 MB。

## 历史：修过的内存泄漏

第二十五轮 `perf-audit` 发现 FEC 组状态**永不回收**：

```text
tracked_groups() 与 frames_in 严格 1:1 增长
30fps/20s -> 601 组，60fps/100s -> 5973 组，永不下降
进程私有内存 +0.27-0.52 MB/s
```

64KB 帧（55 分片）单帧就是一个完整 `FecDecoder` 的分片拷贝，约 64KB。

修法：`prune_stale(current_frame_seq, keep_frames)` 在 1ms tick 里调用，
保留 8 帧（约 260ms @30fps）。实测 `fec_groups_pruned` 逐窗口增长（3498 / 60s），
内存不再线性涨。

## 复现

```powershell
$env:SIDELENS_OPUS = "1"
$env:SIDELENS_H264_DECODE = "1"
$env:SIDELENS_RENDER = "headless"

$d = Start-Process target\release\sidelensd.exe -ArgumentList "--udp 31300 --no-tui" -PassThru
Start-Sleep -Milliseconds 1200
$pid1 = $d.Id

# 分段跑，每段后采样内存
& target\release\sidelenssend.exe --udp 127.0.0.1:31300 60 8192 30 --loss 0.03 --fec 2 --audio --opus --h264 --size 1280x720
[math]::Round((Get-Process -Id $pid1).WorkingSet64/1MB, 1)
```
