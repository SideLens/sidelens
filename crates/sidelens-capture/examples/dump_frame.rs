//! 采集一帧 NV12 存盘，并打印 Y 平面统计证明不是全黑
//!
//! # 输出文件
//!
//! `capture-frame.nv12`（可用 `SIDELENS_DUMP_PATH` 改路径）。裸 NV12，
//! 用 ffmpeg 可以直接看：
//!
//! `text
//! ffmpeg -f rawvideo -pix_fmt nv12 -s 1920x1088 -i capture-frame.nv12 frame.png
//! `
//!
//! **注意 `-s` 要用 coded 尺寸（16 对齐后的），不是显示尺寸** ——
//! 文件里就是 coded 布局，用显示尺寸会得到斜切/花屏的图。
//!
//! # 为什么要打印 min/max/mean
//!
//! 「文件生成了、大小也对」不能证明采集成功：全 16 的帧同样满足这两个条件
//! （16 是 limited range 的黑，不是 0）。min/max/mean 三个数一起看才能判断：
//!
//! - `min == max == 16` → 全黑，采集链路没读到像素
//! - `min == max == 235` → 全白（少见，通常是全屏白窗口）
//! - `min < max` 且 `mean` 在 30..220 → 正常画面

#[cfg(windows)]
mod imp {
    //! Windows 实现

    #![allow(unsafe_code)]

    use std::io::Write;

    use sidelens_capture::{CaptureSource, ScreenCapture};

    /// 按名字挑后端：gdi / dxgi / synthetic
    fn open(backend: &str) -> Result<ScreenCapture, String> {
        match backend {
            "gdi" => ScreenCapture::new_gdi(CaptureSource::PrimaryMonitor)
                .map_err(|e| format!("GDI 打不开：{e}（hresult={:?}）", e.hresult_hex())),
            "dxgi" => ScreenCapture::new_dxgi(CaptureSource::PrimaryMonitor)
                .map_err(|e| format!("DXGI 打不开：{e}（hresult={:?}）", e.hresult_hex())),
            "synthetic" => Ok(ScreenCapture::synthetic(1920, 1080)),
            other => Err(format!("未知后端 {other}，可选 gdi / dxgi / synthetic")),
        }
    }

    pub fn main() {
        let backend = std::env::args().nth(1).unwrap_or_else(|| "gdi".into());
        let path = std::env::var("SIDELENS_DUMP_PATH")
            .unwrap_or_else(|_| "capture-frame.nv12".to_string());

        let mut cap = match open(&backend) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(1);
            }
        };

        // DXGI 在静止桌面下可能连续超时，最多等 1 秒
        let mut frame = None;
        for attempt in 0..60 {
            match cap.capture() {
                Ok(Some(f)) => {
                    frame = Some(f);
                    break;
                }
                Ok(None) => {
                    if attempt == 0 {
                        println!("桌面静止，等待新帧…");
                    }
                    continue;
                }
                Err(e) => {
                    eprintln!("采集失败：{e}（hresult={:?}）", e.hresult_hex());
                    std::process::exit(1);
                }
            }
        }
        let frame = match frame {
            Some(f) => f,
            None => {
                eprintln!("1 秒内没有拿到任何帧");
                std::process::exit(1);
            }
        };

        let stats = frame.y_stats();
        println!("后端       {}", cap.backend());
        println!(
            "显示尺寸   {}x{}",
            frame.display_width, frame.display_height
        );
        println!(
            "编码尺寸   {}x{}（16 对齐，多出 {} 行 / {} 列）",
            frame.coded_width,
            frame.coded_height,
            frame.coded_height - frame.display_height,
            frame.coded_width - frame.display_width
        );
        println!("字节数     {}（coded_w*coded_h*3/2）", frame.data.len());
        println!("采集时刻   {} µs", frame.captured_at_us);
        println!(
            "Y 平面     min={} max={} mean={:.2} samples={}",
            stats.min, stats.max, stats.mean, stats.samples
        );

        let verdict = if frame.is_black() {
            "全黑（min==max==16）—— 采集链路没有读到真实像素"
        } else if stats.max - stats.min < 8 {
            "几乎无动态范围 —— 画面可能是纯色"
        } else {
            "正常（有动态范围）"
        };
        println!("判定       {verdict}");

        match std::fs::File::create(&path) {
            Ok(mut f) => match f.write_all(&frame.data) {
                Ok(()) => println!("已写入     {path}（裸 NV12，用 coded 尺寸喂 ffmpeg）"),
                Err(e) => eprintln!("写文件失败：{e}"),
            },
            Err(e) => eprintln!("创建文件失败：{e}"),
        }

        if frame.is_black() {
            // 全黑是真实故障（不是「桌面本来就是黑的」这种借口能解释的：
            // 用户在跑这个示例时屏幕上显然有内容），必须让退出码非零
            std::process::exit(2);
        }
    }
}

#[cfg(windows)]
fn main() {
    imp::main();
}

#[cfg(not(windows))]
fn main() {
    eprintln!(
        "此示例只支持 Windows 后端；非 Windows 上请用 cargo test -p sidelens-capture 跑合成源测试"
    );
}
