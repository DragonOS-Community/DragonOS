pub struct StdioUtils;

impl StdioUtils {
    /// # 将标准错误输出转换为行列表
    pub fn stderr_to_lines(stderr: &[u8]) -> Vec<String> {
        let stderr = String::from_utf8_lossy(stderr);
        return stderr.lines().map(|s| s.to_string()).collect();
    }

    /// 获取标准错误输出的最后n行, 以字符串形式返回.
    /// 如果标准错误输出的行数小于n, 则返回所有行.
    pub fn tail_n_str(lines: Vec<String>, n: usize) -> String {
        let mut result = String::new();
        let start = if lines.len() > n { lines.len() - n } else { 0 };
        for line in lines.iter().skip(start) {
            result.push_str(line);
            result.push_str("\n");
        }
        return result;
    }
}
