use std::{path::PathBuf, time::Duration};

use anyhow::{anyhow, Result};
use clap::{Parser, Subcommand};

#[derive(Debug, Subcommand, Clone, PartialEq, Eq)]
pub enum ProfileCommand {
    #[clap(about = "Sample the kernel")]
    Sample(ProfileSampleArgs),
    #[clap(about = "Parse the collected sample data")]
    Parse(ProfileParseArgs),
}

#[derive(Debug, Parser, Clone, PartialEq, Eq)]
pub struct ProfileSampleArgs {
    #[clap(
        long = "kernel",
        help = "Path to the kernel image to use",
        default_value = "./bin/kernel/kernel.elf"
    )]
    pub kernel: PathBuf,
    #[clap(
        long = "interval",
        help = "Interval between samples (e.g., 200ms, 1s, 1m)",
        default_value = "200ms",
        value_parser = parse_time_interval
    )]
    interval: Duration,

    #[clap(
        long = "duration",
        help = "Duration of the sampling in seconds",
        default_value = "10s",
        value_parser = parse_time_interval
    )]
    duration: Duration,
    #[clap(long = "output", help = "Path of the output file")]
    pub output: PathBuf,

    #[clap(
        long = "format",
        help = "Output file format (flamegraph, json, folded)",
        default_value = "flamegraph",
        value_parser = parse_profile_file_type
    )]
    pub format: ProfileFileType,

    #[clap(
        long = "remote",
        help = "Remote address to connect to",
        default_value = "localhost:1234"
    )]
    pub remote: String,

    #[clap(
        long = "workers",
        help = "Number of worker threads to use",
        default_value = "3"
    )]
    pub workers: usize,
    #[clap(
        long = "cpu-mask",
        help = "CPU mask to filter",
        value_parser = parse_cpu_mask
    )]
    pub cpu_mask: Option<u128>,
}

impl ProfileSampleArgs {
    pub fn interval(&self) -> Duration {
        self.interval
    }

    pub fn duration(&self) -> Duration {
        self.duration
    }
}

fn parse_time_interval(interval: &str) -> Result<Duration> {
    let interval = interval
        .parse::<humantime::Duration>()
        .map_err(|e| anyhow!("Failed to parse interval: {}, error: {}", interval, e))?;
    Ok(interval.into())
}

fn parse_profile_file_type(format: &str) -> Result<ProfileFileType> {
    match format.trim().to_ascii_lowercase().as_str() {
        "json" => Ok(ProfileFileType::Json),
        "folded" => Ok(ProfileFileType::Folded),
        "flamegraph" => Ok(ProfileFileType::Flamegraph),
        _ => Err(anyhow!("Unknown profile file type: {}", format)),
    }
}

fn parse_cpu_mask(s: &str) -> Result<u128> {
    let mask = if s.starts_with("0x") || s.starts_with("0X") {
        u128::from_str_radix(&s[2..], 16)
    } else {
        s.parse::<u128>()
    };

    let mask = mask.map_err(|e| anyhow!("Failed to parse cpu mask: {}, error: {}", s, e))?;
    Ok(mask)
}

#[derive(Debug, Parser, Clone, PartialEq, Eq)]
pub struct ProfileParseArgs {
    #[clap(
        long = "duration",
        help = "Duration of the sampling in seconds",
        default_value = "10s",
        value_parser = parse_time_interval
    )]
    duration: Duration,
    #[clap(long = "input", help = "Path of the input file")]
    pub input: PathBuf,

    #[clap(long = "output", help = "Path of the output file")]
    pub output: PathBuf,

    #[clap(
        long = "format",
        help = "Output file format (flamegraph, json, folded)",
        default_value = "flamegraph",
        value_parser = parse_profile_file_type
    )]
    pub format: ProfileFileType,

    #[clap(
        long = "cpu-mask",
        help = "CPU mask to filter",
        value_parser = parse_cpu_mask
    )]
    pub cpu_mask: Option<u128>,
}

/// 输出的文件类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileFileType {
    /// Json格式
    Json,
    /// 栈帧折叠格式
    Folded,
    /// 火焰图
    Flamegraph,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_time_interval() {
        assert_eq!(
            parse_time_interval("1ms").unwrap(),
            Duration::from_millis(1)
        );
        assert_eq!(parse_time_interval("1s").unwrap(), Duration::from_secs(1));
        assert_eq!(parse_time_interval("1m").unwrap(), Duration::from_secs(60));
    }

    #[test]
    fn test_parse_cpu_mask() {
        assert_eq!(parse_cpu_mask("1").unwrap(), 1);
        assert_eq!(parse_cpu_mask("0x1").unwrap(), 1);
    }
}
