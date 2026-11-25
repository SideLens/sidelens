//! 发布清单的端到端契约测试。
//!
//! 与 `src/` 里的单元测试不同，这里跑的是**真实文件**：
//! 建一个目录、写几个文件、算真实 sha256、生成清单、再用安装布局
//! 从盘上校验回来。这样能抓到「单元层各自对、拼起来路径错」的问题
//! （比如反斜杠与正斜杠混用、大小写敏感、`join` 把相对路径当绝对路径）。

use std::fs;
use std::path::{Path, PathBuf};

use sidelens_update::{
    decide, hash::sha256_bytes, layout::Layout, manifest::Manifest, manifest::ManifestFile,
    manifest::PhoneManifest, Decision, InstalledRecord, Version,
};

fn scratch(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("sidelens-update-it-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("建临时目录");
    dir
}

/// 造一个假的发布产物目录：两个文件 + 一份清单
fn fake_release(root: &Path, version: &str) -> Manifest {
    fs::write(root.join("sidelensd.exe"), b"sidelensd-binary-payload").unwrap();
    fs::create_dir_all(root.join("sdk")).unwrap();
    fs::write(root.join("sdk").join("sidelens_dshow.dll"), b"dshow-dll").unwrap();

    let manifest = Manifest {
        schema: 1,
        product: "SideLens".into(),
        channel: "stable".into(),
        version: Version::parse(version).unwrap(),
        released: "2026-11-26T00:00:00Z".into(),
        min_upgradable_version: Version::new(0, 1, 0),
        files: vec![
            ManifestFile {
                path: "sidelensd.exe".into(),
                size: 24,
                sha256: sha256_bytes(b"sidelensd-binary-payload"),
            },
            ManifestFile {
                path: "sdk/sidelens_dshow.dll".into(),
                size: 9,
                sha256: sha256_bytes(b"dshow-dll"),
            },
        ],
        phone: PhoneManifest {
            min_supported_version_code: 1,
            latest_version_code: 2,
            latest_version_name: "0.2.0".into(),
            min_supported_protocol: 1,
        },
        notes: "集成测试夹具".into(),
    };
    fs::write(root.join("manifest.json"), manifest.to_json()).unwrap();
    manifest
}

#[test]
fn 真实目录生成清单后能原地校验通过() {
    let dir = scratch("roundtrip");
    let manifest = fake_release(&dir, "0.2.0");

    // 磁盘 → JSON → 结构体，模拟安装器读清单的过程
    let text = fs::read_to_string(dir.join("manifest.json")).unwrap();
    let parsed = Manifest::parse(&text).expect("清单应能解析");
    assert_eq!(parsed, manifest);

    let failures = sidelens_update::verify_tree(&dir, &parsed.files);
    assert!(failures.is_empty(), "不该有失败项：{failures:?}");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn 改动一个字节就被校验抓出来() {
    let dir = scratch("tamper");
    let manifest = fake_release(&dir, "0.2.0");
    // 同长度、不同内容
    fs::write(dir.join("sidelensd.exe"), b"sidelensd-binary-payloaX").unwrap();

    let failures = sidelens_update::verify_tree(&dir, &manifest.files);
    assert_eq!(failures.len(), 1);
    assert_eq!(failures[0].0, "sidelensd.exe");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn 安装布局能把清单里的相对路径落到盘上() {
    let dir = scratch("layout");
    let layout = Layout::new(dir.join("SideLens"));
    fs::create_dir_all(layout.root().join("sdk")).unwrap();
    fs::write(layout.join_relative("sdk/sidelens_dshow.dll"), b"dshow-dll").unwrap();

    assert!(layout.join_relative("sdk/sidelens_dshow.dll").exists());
    assert_eq!(layout.root(), dir.join("SideLens"));
    assert_eq!(
        layout.exe().file_name().unwrap(),
        sidelens_update::layout::exe_name()
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn 安装记录写盘再读回与清单版本一致() {
    let dir = scratch("installed");
    let manifest = fake_release(&dir, "0.2.0");
    let layout = Layout::new(dir.join("SideLens"));
    let record = InstalledRecord::new(manifest.version, &layout, "2026-11-26T00:00:00Z".into());
    fs::create_dir_all(layout.root()).unwrap();
    fs::write(layout.installed(), record.to_json()).unwrap();

    let text = fs::read_to_string(layout.installed()).unwrap();
    let read_back = InstalledRecord::parse(&text).unwrap();
    assert_eq!(read_back.version, manifest.version);
    // 关键断言：用记录里的版本去决策，结论必须是"已最新"
    assert_eq!(
        decide(&read_back.version.to_string(), &manifest),
        Decision::UpToDate {
            current: Version::new(0, 2, 0),
            latest: Version::new(0, 2, 0)
        }
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn 清单里的路径不能逃出安装根() {
    let dir = scratch("escape");
    let value = serde_json::json!({
        "schema": 1,
        "product": "SideLens",
        "version": "0.2.0",
        "files": [
            { "path": "../outside.exe", "size": 1, "sha256": "0".repeat(64) }
        ]
    });
    assert!(Manifest::parse(&value.to_string()).is_err());
    let _ = fs::remove_dir_all(&dir);
}
