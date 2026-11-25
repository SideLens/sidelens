//! 文件哈希与清单校验。
//!
//! 校验分两步，顺序固定：**先比 size，再算 sha256**。
//!
//! 理由：一份被截断的下载里 size 就差，一次 `metadata()` 就能挡掉，
//! 不必把 3 MB 读进来算哈希。而 size 相同的坏文件（位翻转、半截覆盖）
//! 仍然要靠 sha256 —— 所以第二步不能省。

use std::fs::File;
use std::io::{self, Read};
use std::path::Path;

use sha2::{Digest, Sha256};

use crate::manifest::ManifestFile;

/// 计算文件的 sha256，返回 64 位小写十六进制。
///
/// 按 64 KiB 分块读，避免把整个 exe 读进内存 —— 安装目录里的 DLL 加起来
/// 也不大，但这个函数同样会被用在用户指定的大文件上。
pub fn sha256_file(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(to_hex(&hasher.finalize()))
}

/// 计算内存里一段字节的 sha256
pub fn sha256_bytes(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    to_hex(&hasher.finalize())
}

fn to_hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[(byte >> 4) as usize] as char);
        out.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    out
}

/// 一个文件与清单条目的比对结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckError {
    /// 文件不存在
    Missing,
    /// 大小不符
    Size { expected: u64, actual: u64 },
    /// 内容不符
    Hash { expected: String, actual: String },
    /// 读文件出错
    Io(String),
}

impl std::fmt::Display for CheckError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CheckError::Missing => write!(f, "文件不存在"),
            CheckError::Size { expected, actual } => {
                write!(f, "大小不符（清单 {expected} 字节，实际 {actual} 字节）")
            }
            CheckError::Hash { expected, actual } => {
                write!(f, "sha256 不符（清单 {expected}，实际 {actual}）")
            }
            CheckError::Io(e) => write!(f, "读取失败：{e}"),
        }
    }
}

impl std::error::Error for CheckError {}

/// 校验一个文件是否符合清单条目
pub fn verify_file(path: &Path, entry: &ManifestFile) -> Result<(), CheckError> {
    let metadata = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(CheckError::Missing),
        Err(e) => return Err(CheckError::Io(e.to_string())),
    };
    if !metadata.is_file() {
        return Err(CheckError::Missing);
    }
    if metadata.len() != entry.size {
        return Err(CheckError::Size {
            expected: entry.size,
            actual: metadata.len(),
        });
    }
    let actual = sha256_file(path).map_err(|e| CheckError::Io(e.to_string()))?;
    if actual != entry.sha256 {
        return Err(CheckError::Hash {
            expected: entry.sha256.clone(),
            actual,
        });
    }
    Ok(())
}

/// 一次校验整份清单（`base` 为根目录）
pub fn verify_tree(base: &Path, files: &[ManifestFile]) -> Vec<(String, CheckError)> {
    let mut failures = Vec::new();
    for entry in files {
        let path = base.join(entry.path.replace('/', std::path::MAIN_SEPARATOR_STR));
        if let Err(error) = verify_file(&path, entry) {
            failures.push((entry.path.clone(), error));
        }
    }
    failures
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "sidelens-update-test-{}-{name}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    fn entry_for(bytes: &[u8]) -> ManifestFile {
        ManifestFile {
            path: "x.bin".into(),
            size: bytes.len() as u64,
            sha256: sha256_bytes(bytes),
        }
    }

    #[test]
    fn 已知输入的_sha256_与外部工具一致() {
        // echo -n "abc" | sha256sum
        assert_eq!(
            sha256_bytes(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            sha256_bytes(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn 文件哈希与内存哈希一致() {
        let dir = temp_dir("hash");
        let path = dir.join("a.bin");
        fs::write(&path, b"hello world").unwrap();
        assert_eq!(sha256_file(&path).unwrap(), sha256_bytes(b"hello world"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn 校验通过() {
        let dir = temp_dir("ok");
        let path = dir.join("a.bin");
        fs::write(&path, b"payload").unwrap();
        assert_eq!(verify_file(&path, &entry_for(b"payload")), Ok(()));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn 大小不符先于哈希被报出来() {
        let dir = temp_dir("size");
        let path = dir.join("a.bin");
        fs::write(&path, b"short").unwrap();
        match verify_file(&path, &entry_for(b"much longer payload")) {
            Err(CheckError::Size { expected, actual }) => {
                assert_eq!(expected, 19);
                assert_eq!(actual, 5);
            }
            other => panic!("得到 {other:?}"),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn 同大小不同内容靠哈希抓出来() {
        let dir = temp_dir("hash-mismatch");
        let path = dir.join("a.bin");
        fs::write(&path, b"AAAA").unwrap();
        match verify_file(&path, &entry_for(b"BBBB")) {
            Err(CheckError::Hash { expected, actual }) => {
                assert_ne!(expected, actual);
            }
            other => panic!("得到 {other:?}"),
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn 缺文件报缺失而不是_io_错误() {
        let dir = temp_dir("missing");
        let path = dir.join("nope.bin");
        assert_eq!(
            verify_file(&path, &entry_for(b"x")),
            Err(CheckError::Missing)
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn 目录当成缺失处理() {
        let dir = temp_dir("dir-as-file");
        let target = dir.join("sub");
        fs::create_dir_all(&target).unwrap();
        assert_eq!(
            verify_file(&target, &entry_for(b"x")),
            Err(CheckError::Missing)
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn 整树校验列出所有失败项() {
        let dir = temp_dir("tree");
        fs::create_dir_all(dir.join("sdk")).unwrap();
        fs::write(dir.join("a.exe"), b"one").unwrap();
        // sdk/b.dll 故意不建
        let files = vec![
            ManifestFile {
                path: "a.exe".into(),
                size: 3,
                sha256: sha256_bytes(b"one"),
            },
            ManifestFile {
                path: "sdk/b.dll".into(),
                size: 3,
                sha256: sha256_bytes(b"two"),
            },
        ];
        let failures = verify_tree(&dir, &files);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].0, "sdk/b.dll");
        assert_eq!(failures[0].1, CheckError::Missing);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn 错误消息都带具体数字() {
        let size_error = CheckError::Size {
            expected: 10,
            actual: 3,
        };
        assert!(size_error.to_string().contains("10"));
        assert!(size_error.to_string().contains('3'));
        let hash_error = CheckError::Hash {
            expected: "aa".into(),
            actual: "bb".into(),
        };
        assert!(hash_error.to_string().contains("aa"));
    }
}
