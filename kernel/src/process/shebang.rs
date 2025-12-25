// SPDX-License-Identifier: GPL-2.0
//! Shebang script loader
//!
//! This module implements shebang (#!) script parsing and execution,
//! following Linux kernel semantics (fs/binfmt_script.c).

use alloc::string::{String, ToString};
use core::fmt::Debug;

use super::exec::{BinaryLoader, BinaryLoaderResult, ExecError, ExecParam};

/// Shebang行最大长度 (与Linux BINPRM_BUF_SIZE一致)
pub const SHEBANG_MAX_LINE_SIZE: usize = 256;

/// 最大递归深度限制 (防止shebang循环引用)
/// Linux默认为4
pub const SHEBANG_MAX_RECURSION_DEPTH: usize = 4;

/// Shebang魔数
pub const SHEBANG_MAGIC: [u8; 2] = [b'#', b'!'];

/// Shebang解析结果
#[derive(Debug, Clone)]
pub struct ShebangInfo {
    /// 解释器路径
    pub interpreter_path: String,
    /// 解释器的可选参数 (shebang行中的第一个参数)
    pub interpreter_arg: Option<String>,
}

/// Shebang加载器
#[derive(Debug)]
pub struct ShebangLoader;

/// 全局Shebang加载器实例
pub const SHEBANG_LOADER: ShebangLoader = ShebangLoader;

impl ShebangLoader {
    /// 解析shebang行
    ///
    /// ## 参数
    /// - `buf`: 文件头部数据
    ///
    /// ## 返回值
    /// - Ok(ShebangInfo): 解析成功
    /// - Err(ExecError): 解析失败或格式错误
    ///
    /// ## Linux语义
    /// - shebang行格式: `#!interpreter [optional-arg]`
    /// - 最大长度256字节
    /// - 只支持一个可选参数
    #[inline(never)]
    pub fn parse_shebang_line(buf: &[u8]) -> Result<ShebangInfo, ExecError> {
        // 1. 检查魔数 "#!"
        if buf.len() < 2 || buf[0] != SHEBANG_MAGIC[0] || buf[1] != SHEBANG_MAGIC[1] {
            return Err(ExecError::NotExecutable);
        }

        // 2. 查找行尾 (换行符或缓冲区结束)
        let max_len = SHEBANG_MAX_LINE_SIZE.min(buf.len());
        let line_end = buf[2..max_len]
            .iter()
            .position(|&c| c == b'\n' || c == b'\r')
            .map(|pos| pos + 2)
            .unwrap_or(max_len);

        // 3. 提取shebang行内容 (去掉 "#!" 前缀)
        let line = &buf[2..line_end];

        // 4. 跳过前导空白
        let line_start = line
            .iter()
            .position(|&c| c != b' ' && c != b'\t')
            .unwrap_or(line.len());

        let line = &line[line_start..];

        if line.is_empty() {
            return Err(ExecError::NotExecutable);
        }

        // 5. 提取解释器路径 (到第一个空白字符)
        let interp_end = line
            .iter()
            .position(|&c| c == b' ' || c == b'\t')
            .unwrap_or(line.len());

        let interpreter_path = core::str::from_utf8(&line[..interp_end])
            .map_err(|_| ExecError::ParseError)?
            .to_string();

        if interpreter_path.is_empty() {
            return Err(ExecError::NotExecutable);
        }

        // 6. 提取可选参数 (如果存在)
        let interpreter_arg = if interp_end < line.len() {
            // 跳过解释器路径后的空白
            let remaining = &line[interp_end..];
            let arg_start = remaining.iter().position(|&c| c != b' ' && c != b'\t');

            if let Some(start) = arg_start {
                let arg_line = &remaining[start..];
                // 提取参数 (到下一个空白或行尾)
                let arg_end = arg_line
                    .iter()
                    .position(|&c| c == b' ' || c == b'\t')
                    .unwrap_or(arg_line.len());

                let arg = core::str::from_utf8(&arg_line[..arg_end])
                    .map_err(|_| ExecError::ParseError)?
                    .to_string();

                if !arg.is_empty() {
                    Some(arg)
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };

        Ok(ShebangInfo {
            interpreter_path,
            interpreter_arg,
        })
    }
}

impl BinaryLoader for ShebangLoader {
    /// 检查是否为shebang脚本
    fn probe(&'static self, _param: &ExecParam, buf: &[u8]) -> Result<(), ExecError> {
        if buf.len() >= 2 && buf[0] == SHEBANG_MAGIC[0] && buf[1] == SHEBANG_MAGIC[1] {
            Ok(())
        } else {
            Err(ExecError::NotExecutable)
        }
    }

    /// Shebang加载器不直接加载二进制文件
    /// 它通过返回NeedReexec来触发递归执行
    fn load(
        &'static self,
        _param: &mut ExecParam,
        _head_buf: &[u8],
    ) -> Result<BinaryLoaderResult, ExecError> {
        // Shebang的实际处理在load_binary_file中完成
        // 这里不应该被直接调用
        Err(ExecError::NotSupported)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_simple_shebang() {
        let buf = b"#!/bin/sh\necho hello";
        let info = ShebangLoader::parse_shebang_line(buf).unwrap();
        assert_eq!(info.interpreter_path, "/bin/sh");
        assert_eq!(info.interpreter_arg, None);
    }

    #[test]
    fn test_parse_shebang_with_arg() {
        let buf = b"#!/usr/bin/env python3\nprint('hello')";
        let info = ShebangLoader::parse_shebang_line(buf).unwrap();
        assert_eq!(info.interpreter_path, "/usr/bin/env");
        assert_eq!(info.interpreter_arg, Some("python3".to_string()));
    }

    #[test]
    fn test_parse_shebang_with_spaces() {
        let buf = b"#!  /bin/bash  -x\necho hello";
        let info = ShebangLoader::parse_shebang_line(buf).unwrap();
        assert_eq!(info.interpreter_path, "/bin/bash");
        assert_eq!(info.interpreter_arg, Some("-x".to_string()));
    }

    #[test]
    fn test_parse_invalid_shebang() {
        let buf = b"#!/\necho hello";
        let result = ShebangLoader::parse_shebang_line(buf);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_no_shebang() {
        let buf = b"echo hello";
        let result = ShebangLoader::parse_shebang_line(buf);
        assert!(result.is_err());
    }
}
