use anyhow::{Context, Result};
use std::{
    fs,
    path::{Path, PathBuf},
};

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[derive(Debug)]
pub struct Manifest {
    pub tests: Vec<TestSpec>,
}

#[derive(Debug, Clone)]
pub struct TestSpec {
    pub name: String,
    pub path: String,
    pub args: Vec<String>,
    pub timeout_sec: u64,
}

impl Manifest {
    pub fn discover(bin_dir: &Path, default_timeout_sec: u64) -> Result<Self> {
        if !bin_dir.is_dir() {
            anyhow::bail!("测试二进制目录不存在: {}", bin_dir.display());
        }
        let mut tests = Vec::new();
        discover_in_dir(bin_dir, bin_dir, default_timeout_sec, &mut tests)?;

        tests.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Manifest { tests })
    }
}

fn discover_in_dir(
    root: &Path,
    current: &Path,
    default_timeout_sec: u64,
    tests: &mut Vec<TestSpec>,
) -> Result<()> {
    let entries = fs::read_dir(current)
        .with_context(|| format!("读取测试二进制目录失败: {}", current.display()))?;

    for entry in entries {
        let entry = entry.with_context(|| "读取目录项失败")?;
        let path = entry.path();
        if path.is_dir() {
            discover_in_dir(root, &path, default_timeout_sec, tests)?;
            continue;
        }
        if !path.is_file() || !is_executable(&path)? {
            continue;
        }

        let Some(rel) = to_relative_slash(root, &path) else {
            continue;
        };

        tests.push(TestSpec {
            name: normalize_case_name(&rel),
            path: path.display().to_string(),
            args: Vec::new(),
            timeout_sec: default_timeout_sec,
        });
    }
    Ok(())
}

fn to_relative_slash(root: &Path, path: &Path) -> Option<String> {
    let rel = path.strip_prefix(root).ok()?;
    let mut s = String::new();
    for (idx, part) in rel.components().enumerate() {
        if idx > 0 {
            s.push('/');
        }
        s.push_str(part.as_os_str().to_str()?);
    }
    Some(s)
}

fn normalize_case_name(relative_path: &str) -> String {
    let path = PathBuf::from(relative_path);
    let mut parts: Vec<String> = path
        .iter()
        .map(|seg| seg.to_string_lossy().to_string())
        .collect();

    if let Some(last) = parts.last_mut() {
        if let Some(stripped) = last.strip_suffix("_test") {
            *last = stripped.to_string();
        }
    }
    parts.join("/")
}

fn is_executable(path: &Path) -> Result<bool> {
    #[cfg(unix)]
    {
        let metadata = fs::metadata(path)
            .with_context(|| format!("读取文件属性失败: {}", path.display()))?;
        Ok(metadata.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(true)
    }
}
