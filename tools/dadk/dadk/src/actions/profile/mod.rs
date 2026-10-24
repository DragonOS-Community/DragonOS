use std::{
    collections::{BTreeMap, HashMap},
    io::{Read, Write},
    path::PathBuf,
    process::Command,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, Weak,
    },
};

use crate::{
    console::profile::{ProfileCommand, ProfileFileType, ProfileParseArgs, ProfileSampleArgs},
    context::DADKExecContext,
};

use anyhow::{anyhow, Result};
use indicatif::{ProgressBar, ProgressStyle};
use lazy_static::lazy_static;
use rayon::ThreadPoolBuilder;
use serde::{Deserialize, Serialize};

lazy_static! {
    static ref GUEST_ADDRESS_HEX_PATTERN: regex::Regex =
        regex::Regex::new(r"0x[0-9a-fA-F]+ in").unwrap();
    static ref RUST_IMPL_PATTERN: regex::Regex = regex::Regex::new(r"::\{.*?\}").unwrap();
}

pub(super) fn run(ctx: &DADKExecContext, cmd: &ProfileCommand) -> Result<()> {
    match cmd {
        ProfileCommand::Sample(profile_sample_args) => sample(ctx, profile_sample_args),
        ProfileCommand::Parse(profile_parse_args) => parse_input_data(ctx, profile_parse_args),
    }
}

fn sample(_ctx: &DADKExecContext, args: &ProfileSampleArgs) -> Result<()> {
    let profiler = Profiler::new(args.clone());
    profiler.run()?;
    profiler.save()
}

fn parse_input_data(_ctx: &DADKExecContext, args: &ProfileParseArgs) -> Result<()> {
    let sample_buf =
        SampleBuffer::from_saved_file(&args.input).expect("Failed to load sample buffer");
    sample_buf.export_data(args.format, &args.output, args.cpu_mask);
    log::info!("Profile data saved to {}", args.output.display());
    Ok(())
}

/// 一个时刻的采样数据
#[derive(Debug, Serialize, Deserialize, Clone)]
struct Sample {
    /// The sample data
    /// The key is the cpu id
    /// The value is the sample data
    data: BTreeMap<usize, Vec<String>>,
    id: usize,
    timestamp: usize,
    #[serde(skip)]
    current_cpu: Option<usize>,
}

impl Sample {
    fn new(id: usize, timestamp: usize) -> Self {
        Self {
            data: BTreeMap::new(),
            id,
            timestamp,
            current_cpu: None,
        }
    }

    fn push_new_line(&mut self, line: &str) {
        if line.starts_with("#") {
            self.parse_frame_line(line);
        } else {
            self.parse_thread_line(line);
        }
    }

    fn parse_frame_line(&mut self, line: &str) {
        let line = line.trim();
        // todo: 支持调整删除的`<>`的层级，以便打印更详细的信息
        let line = remove_angle_bracket_content(&line);
        let line = remove_guest_address(&line);
        let mut line = remove_rust_impl_pattern(&line);
        line = line.replace("(...)", "");
        line = line.replace("()", "");

        let parts = line.split_whitespace().collect::<Vec<_>>();
        if parts.len() >= 2 {
            let fn_name = parts[1];
            self.data
                .get_mut(&self.current_cpu.unwrap())
                .unwrap()
                .push(fn_name.to_string());
        }
    }

    fn parse_thread_line(&mut self, line: &str) {
        if line.starts_with("Thread") {
            let idx = line.find("CPU#").unwrap();
            self.current_cpu = Some(
                line[idx + 4..]
                    .split_whitespace()
                    .next()
                    .unwrap()
                    .parse::<usize>()
                    .unwrap(),
            );

            if !self.data.contains_key(&self.current_cpu.unwrap()) {
                self.data.insert(self.current_cpu.unwrap(), Vec::new());
            } else {
                log::error!(
                    "current cpu {} is already set in hashmap",
                    self.current_cpu.unwrap()
                );
            }
        }
    }

    #[allow(dead_code)]
    fn vcpu_count(&self) -> usize {
        self.data.len()
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct SampleBuffer {
    samples: Vec<Sample>,
}

impl SampleBuffer {
    fn new() -> Self {
        Self {
            samples: Vec::new(),
        }
    }

    fn push(&mut self, sample: Sample) {
        self.samples.push(sample);
    }

    fn export_data(&self, t: ProfileFileType, outpath: &PathBuf, cpumask: Option<u128>) {
        let mut writer = std::fs::File::create(outpath).unwrap();
        match t {
            ProfileFileType::Json => {
                let filtered = self.filter_cpu(cpumask);
                serde_json::to_writer(&mut writer, &filtered).unwrap();
            }
            ProfileFileType::Folded => {
                let folded = self.fold(cpumask);
                writer.write(folded.to_string().as_bytes()).unwrap();
            }
            ProfileFileType::Flamegraph => {
                let folded = self.fold(cpumask);
                let lines: Vec<String> = folded
                    .data
                    .iter()
                    .map(|(k, cnt)| format!("{} {}", k, cnt))
                    .collect();

                let mut opt = inferno::flamegraph::Options::default();
                inferno::flamegraph::from_lines(&mut opt, lines.iter().map(|s| s.as_str()), writer)
                    .unwrap();
            }
        }
    }

    fn filter_cpu(&self, cpumask: Option<u128>) -> SampleBuffer {
        let cpumask = cpumask.unwrap_or(u128::MAX);
        let mut result = SampleBuffer::new();
        self.samples.iter().for_each(|s| {
            let mut sample = Sample::new(s.id, s.timestamp);
            s.data.iter().for_each(|(cpu, stack)| {
                if *cpu < 128 && (cpumask & (1 << cpu) != 0) {
                    sample.data.insert(*cpu, stack.clone());
                }
            });
            result.push(sample);
        });

        result
    }

    fn fold(&self, cpumask: Option<u128>) -> FoldedSampleBuffer {
        let mut folded_buffer = FoldedSampleBuffer::default();
        let cpumask = cpumask.unwrap_or(u128::MAX);

        for sample in &self.samples {
            for (cpu, stack) in &sample.data {
                if *cpu < 128 && (cpumask & (1 << *cpu)) != 0 {
                    let folded_stack = stack.iter().rev().cloned().collect::<Vec<_>>().join(";");
                    if let Some(cnt) = folded_buffer.data.get_mut(&folded_stack) {
                        *cnt += 1;
                    } else {
                        folded_buffer.data.insert(folded_stack, 1);
                    }
                }
            }
        }

        folded_buffer
    }

    fn from_saved_file(path: &PathBuf) -> Result<Self> {
        let mut file = std::fs::File::open(path)?;
        let mut buf = String::new();
        file.read_to_string(&mut buf)?;
        let samples = serde_json::from_str::<SampleBuffer>(&buf).ok();
        if let Some(samples) = samples {
            return Ok(samples);
        }

        // check if it is a folded file
        let folded = FoldedSampleBuffer::try_from(&buf)?;
        Ok(folded.into())
    }
}

struct Profiler {
    samples: Mutex<SampleBuffer>,
    self_ref: Weak<Profiler>,

    args: ProfileSampleArgs,
}

impl Profiler {
    fn new(args: ProfileSampleArgs) -> Arc<Profiler> {
        Arc::new_cyclic(|self_ref| Self {
            samples: Mutex::new(SampleBuffer::new()),
            args,
            self_ref: self_ref.clone(),
        })
    }

    fn run(&self) -> Result<()> {
        let thread_pool = ThreadPoolBuilder::default()
            .num_threads(self.args.workers)
            .build()
            .map_err(|e| anyhow!("failed to build thread pool: {}", e))?;
        let duration = self.args.duration();
        let interval = self.args.interval();

        // Create a channel for communication
        let (sender, receiver) = crossbeam::channel::unbounded::<Option<Sample>>();
        let mut id = 0;
        let maxid = (duration.as_millis() / interval.as_millis()) as usize;

        let rx_handle = {
            let p = self.self_ref.upgrade().unwrap();

            std::thread::spawn(move || {
                let pb = ProgressBar::new(maxid as u64);
                pb.set_style(
                    ProgressStyle::default_bar()
                        .template(
                            "{spinner:.green} [{elapsed_precise}] [{bar:40.cyan/blue}] {pos}/{len} ({eta})",
                        )
                        .unwrap()
                        .progress_chars("#>-"),
                );
                let mut guard = p.samples.lock().unwrap();
                while guard.samples.len() < maxid {
                    let sample = receiver.recv().ok().flatten();
                    if let Some(sample) = sample {
                        guard.push(sample);
                        pb.inc(1);
                    } else {
                        break;
                    }
                }
            })
        };
        let rx_exited = Arc::new(AtomicBool::new(false));
        let generator_handle = {
            let rxe = rx_exited.clone();
            let p = self.self_ref.upgrade().unwrap();
            std::thread::spawn(move || {
                while id < maxid {
                    if rxe.load(Ordering::SeqCst) {
                        break;
                    }
                    let sd = sender.clone();
                    let pp = p.clone();
                    thread_pool.spawn_fifo(move || {
                        if let Ok(sample) = pp.do_sample_one(id) {
                            sd.send(Some(sample)).unwrap();
                        } else {
                            sd.send(None).unwrap();
                        }
                    });

                    id += 1;
                    std::thread::sleep(interval);
                }
            })
        };
        rx_handle.join().unwrap();
        rx_exited.store(true, Ordering::SeqCst);
        generator_handle.join().unwrap();

        Ok(())
    }

    fn save(&self) -> Result<()> {
        self.samples.lock().unwrap().export_data(
            self.args.format,
            &self.args.output,
            self.args.cpu_mask,
        );
        Ok(())
    }

    fn kernel_path(&self) -> &PathBuf {
        &self.args.kernel
    }

    fn remote(&self) -> &str {
        &self.args.remote
    }

    fn do_sample_one(&self, id: usize) -> Result<Sample> {
        let output = Command::new("gdb")
            .args([
                "-batch",
                "-ex",
                "set pagination off",
                "-ex",
                "set logging file /dev/null",
                "-ex",
                &format!("file {}", &self.kernel_path().display()),
                "-ex",
                &format!("target remote {}", &self.remote()),
                "-ex",
                "thread apply all bt -frame-arguments presence -frame-info short-location",
            ])
            .output()
            .map_err(|e| anyhow::anyhow!("[sample {}]: failed to execute gdb: {}", id, e))?;

        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as usize;
        let mut sample = Sample::new(id, timestamp);

        for line in String::from_utf8_lossy(&output.stdout).lines() {
            sample.push_new_line(line);
        }

        Ok(sample)
    }
}

#[derive(Debug, Default)]
struct FoldedSampleBuffer {
    /// The folded sample data
    /// key: Stack trace (separated by `;`)
    /// value: The number of occurrences of such stack frames
    data: HashMap<String, usize>,
}
impl FoldedSampleBuffer {
    pub fn try_from<T: AsRef<str>>(s: T) -> Result<Self> {
        let s = s.as_ref();
        let mut data = HashMap::new();

        for line in s.lines() {
            let parts: Vec<&str> = line.split(' ').collect();
            if parts.len() != 2 {
                return Err(anyhow!("Invalid format"));
            }

            let key = parts[0].trim().to_string();
            let value = parts[1]
                .trim()
                .parse::<usize>()
                .map_err(|_| anyhow!("Invalid number"))?;

            data.insert(key, value);
        }

        Ok(FoldedSampleBuffer { data })
    }
}

impl ToString for FoldedSampleBuffer {
    fn to_string(&self) -> String {
        let lines: Vec<String> = self
            .data
            .iter()
            .map(|(k, v)| format!("{} {}", k, v))
            .collect();
        lines.join("\n")
    }
}

impl Into<SampleBuffer> for FoldedSampleBuffer {
    fn into(self) -> SampleBuffer {
        let mut samples = SampleBuffer::new();
        for (stack, count) in self.data {
            let mut sample = Sample::new(0, 0);
            for frame in stack.split(';').rev() {
                sample.push_new_line(frame);
            }
            for _ in 0..count {
                samples.push(sample.clone());
            }
        }
        samples
    }
}

/// Removes content within angle brackets from the input string.
///
/// This function iterates through each character in the input string and
/// removes any characters that are inside angle brackets (`<` and `>`).
/// Nested brackets are handled correctly by maintaining a count of open
/// brackets. Characters outside of any brackets are added to the result.
///
/// # Arguments
///
/// * `input` - A string slice that holds the input string to be processed.
///
/// # Returns
///
/// A new `String` with the content inside angle brackets removed.
fn remove_angle_bracket_content(input: &str) -> String {
    let mut result = String::new();
    let mut inside_brackets = 0;

    for c in input.chars() {
        if c == '<' {
            inside_brackets += 1;
            continue;
        } else if c == '>' {
            inside_brackets -= 1;
            continue; // Skip the closing bracket
        }

        // TODO: 支持调整层级数，以便打印更精细的信息？
        if inside_brackets == 0 {
            result.push(c);
        }
    }

    result
}

fn remove_guest_address(input: &str) -> String {
    GUEST_ADDRESS_HEX_PATTERN.replace_all(input, "").to_string()
}

fn remove_rust_impl_pattern(input: &str) -> String {
    RUST_IMPL_PATTERN.replace_all(input, "").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_remove_angle_bracket_content_no_brackets() {
        let input = "Hello, World!";
        let expected = "Hello, World!";
        assert_eq!(remove_angle_bracket_content(input), expected);
    }

    #[test]
    fn test_remove_angle_bracket_content_single_pair() {
        let input = "Hello <World>!";
        let expected = "Hello !";
        assert_eq!(remove_angle_bracket_content(input), expected);
    }

    #[test]
    fn test_remove_angle_bracket_content_multiple_pairs() {
        let input = "Hello <World> <Again>!";
        let expected = "Hello  !";
        assert_eq!(remove_angle_bracket_content(input), expected);
    }

    #[test]
    fn test_remove_angle_bracket_content_nested_brackets() {
        let input = "Hello <W<or>ld>!";
        let expected = "Hello !";
        assert_eq!(remove_angle_bracket_content(input), expected);
    }
    #[test]
    fn test_remove_angle_bracket_content_unmatched_brackets() {
        let input = "Hello <World!";
        let expected = "Hello ";
        assert_eq!(remove_angle_bracket_content(input), expected);
    }

    #[test]
    fn test_rust_impl_pattern() {
        let line = "#2  alloc::sync::{impl#37}::drop<dragonos_kernel::process::ProcessControlBlock, alloc::alloc::Global> (...)";
        let expected: &str = "#2  alloc::sync::drop<dragonos_kernel::process::ProcessControlBlock, alloc::alloc::Global> (...)";
        assert_eq!(remove_rust_impl_pattern(line), expected);
    }

    #[test]
    fn test_guest_address_hex_pattern() {
        let line = "#7  0xffff800001080320 in _ZN15dragonos_kernel4arch6x86_647process5table11TSS_MANAGER17hfcb0efdd9e498178E.llvm.3349419859655245662 ()";
        let expected = "#7   _ZN15dragonos_kernel4arch6x86_647process5table11TSS_MANAGER17hfcb0efdd9e498178E.llvm.3349419859655245662 ()";
        assert_eq!(remove_guest_address(line), expected);
    }

    #[test]
    fn test_profile_parse_one_sample() {
        let stack = r#"
Thread 2 (Thread 1.2 (CPU#1 [halted ])):
#0  core::ptr::non_null::NonNull<alloc::sync::ArcInner<dragonos_kernel::process::ProcessControlBlock>>::as_ref<alloc::sync::ArcInner<dragonos_kernel::process::ProcessControlBlock>> (...)
#1  alloc::sync::Arc<dragonos_kernel::process::ProcessControlBlock, alloc::alloc::Global>::inner<dragonos_kernel::process::ProcessControlBlock, alloc::alloc::Global> (...)
#2  alloc::sync::{impl#37}::drop<dragonos_kernel::process::ProcessControlBlock, alloc::alloc::Global> (...)
#3  core::ptr::drop_in_place<alloc::sync::Arc<dragonos_kernel::process::ProcessControlBlock, alloc::alloc::Global>> ()
#4  dragonos_kernel::process::ProcessManager::arch_idle_func ()
#5  0xffff80001ff94800 in ?? ()
#6  0x0000000000020097 in ?? ()
#7  0xffff800001080320 in _ZN15dragonos_kernel4arch6x86_647process5table11TSS_MANAGER17hfcb0efdd9e498178E.llvm.3349419859655245662 ()
#8  0x00000000f7b82223 in ?? ()
#9  0x00000000178bfbff in ?? ()
#10 0x0000000001020800 in ?? ()
#11 0x0000000000000096 in ?? ()
#12 0xffff80001ff94c28 in ?? ()
#13 0x0000000000000010 in ?? ()
#14 0x0000000000000010 in ?? ()
#15 0x00000000000306a9 in ?? ()
#16 0x00000000000306a9 in ?? ()
#17 0xffff800001080320 in _ZN15dragonos_kernel4arch6x86_647process5table11TSS_MANAGER17hfcb0efdd9e498178E.llvm.3349419859655245662 ()
#18 0xffff80001ff94c38 in ?? ()
#19 0xffff80001ff8bf58 in ?? ()
#20 0xffff80001ff8bf50 in ?? ()
#21 0xffff80001ff8bf88 in ?? ()
#22 0xffff80001ff94c28 in ?? ()
#23 0xffff8000001e196a in dragonos_kernel::smp::init::smp_ap_start_stage2 ()
#24 0x0000000000000001 in ?? ()
#25 0xffff800000182638 in dragonos_kernel::arch::x86_64::smp::smp_ap_start_stage1 ()
#26 0x0000000000000000 in ?? ()

Thread 1 (Thread 1.1 (CPU#0 [running])):
#0  core::sync::atomic::AtomicUsize::fetch_update<fn(usize) -> core::option::Option<usize>> (...)
#1  alloc::sync::Weak<dragonos_kernel::process::ProcessControlBlock, alloc::alloc::Global>::upgrade<dragonos_kernel::process::ProcessControlBlock, alloc::alloc::Global> (...)
#2  dragonos_kernel::process::ProcessControlBlock::arch_current_pcb ()
#3  dragonos_kernel::process::ProcessManager::current_pcb ()
#4  0xffff80001f988de8 in ?? ()
#5  0xffff80001f988de8 in ?? ()
#6  0xffff80001f988dd0 in ?? ()
#7  0x0000000000000000 in ?? ()
        "#;
        let mut sample = Sample::new(0, 0);
        for line in stack.lines() {
            sample.push_new_line(line);
        }
        assert_eq!(sample.vcpu_count(), 2);
        assert_eq!(sample.data.get(&0).unwrap().len(), 8);
        assert_eq!(sample.data.get(&1).unwrap().len(), 27);

        assert_eq!(
            sample.data.get(&0).unwrap()[0],
            "core::sync::atomic::AtomicUsize::fetch_update"
        );
        assert_eq!(
            sample.data.get(&1).unwrap()[0],
            "core::ptr::non_null::NonNull::as_ref"
        );
        println!("{:?}", sample);
    }
}
