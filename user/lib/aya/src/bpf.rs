use core::{ffi::c_int, mem, slice};
use std::{
    borrow::{Cow, ToOwned},
    collections::{HashMap, HashSet},
    fs, io,
    os::fd::{AsFd, AsRawFd, OwnedFd},
    path::{Path, PathBuf},
    string::String,
    sync::Arc,
    vec::Vec,
};

use aya_obj::{
    btf::{Btf, BtfError, BtfFeatures, BtfRelocationError},
    generated::{bpf_map_type::*, *},
    maps::PinningType,
    relocation::EbpfRelocationError,
    EbpfSectionKind, Features, Object, ParseError, ProgramSection,
};
use log::{debug, warn};
use thiserror::Error;

use crate::{
    maps::{Map, MapData, MapError},
    programs::{
        extension::Extension, kprobe::KProbe, probe::ProbeKind, Program, ProgramData, ProgramError,
    },
    sys::*,
    util::{possible_cpus, POSSIBLE_CPUS},
};

pub(crate) const BPF_OBJ_NAME_LEN: usize = 16;

pub(crate) const PERF_EVENT_IOC_ENABLE: c_int = AYA_PERF_EVENT_IOC_ENABLE;
pub(crate) const PERF_EVENT_IOC_DISABLE: c_int = AYA_PERF_EVENT_IOC_DISABLE;
pub(crate) const PERF_EVENT_IOC_SET_BPF: c_int = AYA_PERF_EVENT_IOC_SET_BPF;

lazy_static::lazy_static! {
    pub(crate) static ref FEATURES: Features = detect_features();
}

fn detect_features() -> Features {
    let btf = if is_btf_supported() {
        Some(BtfFeatures::new(
            is_btf_func_supported(),
            is_btf_func_global_supported(),
            is_btf_datasec_supported(),
            is_btf_float_supported(),
            is_btf_decl_tag_supported(),
            is_btf_type_tag_supported(),
            is_btf_enum64_supported(),
        ))
    } else {
        None
    };
    let f = Features::new(
        is_prog_name_supported(),
        is_probe_read_kernel_supported(), // todo! kernel should support helper probe_read_kernel
        false,
        is_bpf_global_data_supported(),
        is_bpf_cookie_supported(), // todo! kernel should support helper bpf_get_attach_cookie
        is_prog_id_supported(BPF_MAP_TYPE_CPUMAP),
        is_prog_id_supported(BPF_MAP_TYPE_DEVMAP),
        btf,
    );
    info!("BPF Feature Detection: {:#?}", f);
    f
}

/// Returns a reference to the detected BPF features.
pub fn features() -> &'static Features {
    &FEATURES
}

/// Builder style API for advanced loading of eBPF programs.
///
/// Loading eBPF code involves a few steps, including loading maps and applying
/// relocations. You can use `EbpfLoader` to customize some of the loading
/// options.
///
/// # Examples
///
/// ```no_run
/// use aya::{EbpfLoader, Btf};
/// use std::fs;
///
/// let bpf = EbpfLoader::new()
///     // load the BTF data from /sys/kernel/btf/vmlinux
///     .btf(Btf::from_sys_fs().ok().as_ref())
///     // load pinned maps from /sys/fs/bpf/my-program
///     .map_pin_path("/sys/fs/bpf/my-program")
///     // finally load the code
///     .load_file("file.o")?;
/// # Ok::<(), aya::EbpfError>(())
/// ```
#[derive(Debug)]
pub struct EbpfLoader<'a> {
    btf: Option<Cow<'a, Btf>>,
    map_pin_path: Option<PathBuf>,
    globals: std::collections::HashMap<&'a str, (&'a [u8], bool)>,
    max_entries: HashMap<&'a str, u32>,
    extensions: HashSet<&'a str>,
    verifier_log_level: VerifierLogLevel,
    allow_unsupported_maps: bool,
}

bitflags::bitflags! {
    /// Used to set the verifier log level flags in [EbpfLoader](EbpfLoader::verifier_log_level()).
    #[derive(Clone, Copy, Debug)]
    pub struct VerifierLogLevel: u32 {
        /// Sets no verifier logging.
        const DISABLE = 0;
        /// Enables debug verifier logging.
        const DEBUG = 1;
        /// Enables verbose verifier logging.
        const VERBOSE = 2 | Self::DEBUG.bits();
        /// Enables verifier stats.
        const STATS = 4;
    }
}

impl Default for VerifierLogLevel {
    fn default() -> Self {
        Self::DEBUG | Self::STATS
    }
}

impl<'a> EbpfLoader<'a> {
    /// Creates a new loader instance.
    pub fn new() -> Self {
        Self {
            btf: None,
            map_pin_path: None,
            globals: std::collections::HashMap::new(),
            max_entries: HashMap::new(),
            extensions: HashSet::new(),
            verifier_log_level: VerifierLogLevel::default(),
            allow_unsupported_maps: false,
        }
    }
    /// Sets the target [BTF](Btf) info.
    ///
    /// The loader defaults to loading `BTF` info using [Btf::from_sys_fs].
    /// Use this method if you want to load `BTF` from a custom location or
    /// pass `None` to disable `BTF` relocations entirely.
    /// # Example
    ///
    /// ```no_run
    /// use aya::{EbpfLoader, Btf, Endianness};
    ///
    /// let bpf = EbpfLoader::new()
    ///     // load the BTF data from a custom location
    ///     .btf(Btf::parse_file("/custom_btf_file", Endianness::default()).ok().as_ref())
    ///     .load_file("file.o")?;
    ///
    /// # Ok::<(), aya::EbpfError>(())
    /// ```
    pub fn btf(&mut self, btf: Option<&'a Btf>) -> &mut Self {
        self.btf = btf.map(Cow::Borrowed);
        self
    }

    /// Allows programs containing unsupported maps to be loaded.
    ///
    /// By default programs containing unsupported maps will fail to load. This
    /// method can be used to configure the loader so that unsupported maps will
    /// be loaded, but won't be accessible from userspace. Can be useful when
    /// using unsupported maps that are only accessed from eBPF code and don't
    /// require any userspace interaction.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use aya::EbpfLoader;
    ///
    /// let bpf = EbpfLoader::new()
    ///     .allow_unsupported_maps()
    ///     .load_file("file.o")?;
    /// # Ok::<(), aya::EbpfError>(())
    /// ```
    ///
    pub fn allow_unsupported_maps(&mut self) -> &mut Self {
        self.allow_unsupported_maps = true;
        self
    }
    /// Sets the base directory path for pinned maps.
    ///
    /// Pinned maps will be loaded from `path/MAP_NAME`.
    /// The caller is responsible for ensuring the directory exists.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use aya::EbpfLoader;
    ///
    /// let bpf = EbpfLoader::new()
    ///     .map_pin_path("/sys/fs/bpf/my-program")
    ///     .load_file("file.o")?;
    /// # Ok::<(), aya::EbpfError>(())
    /// ```
    ///
    pub fn map_pin_path<P: AsRef<Path>>(&mut self, path: P) -> &mut Self {
        self.map_pin_path = Some(path.as_ref().to_owned());
        self
    }
    /// Sets the value of a global variable.
    ///
    /// If the `must_exist` argument is `true`, [`EbpfLoader::load`] will fail with [`ParseError::SymbolNotFound`] if the loaded object code does not contain the variable.
    ///
    /// From Rust eBPF, a global variable can be defined as follows:
    ///
    /// ```no_run
    /// #[no_mangle]
    /// static VERSION: i32 = 0;
    /// ```
    ///
    /// Then it can be accessed using `core::ptr::read_volatile`:
    ///
    /// ```no_run
    /// # #[no_mangle]
    /// # static VERSION: i32 = 0;
    /// # unsafe fn try_test() {
    /// let version = core::ptr::read_volatile(&VERSION);
    /// # }
    /// ```
    ///
    /// The type of a global variable must be `Pod` (plain old data), for instance `u8`, `u32` and
    /// all other primitive types. You may use custom types as well, but you must ensure that those
    /// types are `#[repr(C)]` and only contain other `Pod` types.
    ///
    /// From C eBPF, you would annotate a global variable as `volatile const`.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use aya::EbpfLoader;
    ///
    /// let bpf = EbpfLoader::new()
    ///     .set_global("VERSION", &2, true)
    ///     .set_global("PIDS", &[1234u16, 5678], true)
    ///     .load_file("file.o")?;
    /// # Ok::<(), aya::EbpfError>(())
    /// ```
    ///
    pub fn set_global<T: Into<GlobalData<'a>>>(
        &mut self,
        name: &'a str,
        value: T,
        must_exist: bool,
    ) -> &mut Self {
        self.globals.insert(name, (value.into().bytes, must_exist));
        self
    }

    /// Set the max_entries for specified map.
    ///
    /// Overwrite the value of max_entries of the map that matches
    /// the provided name before the map is created.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use aya::EbpfLoader;
    ///
    /// let bpf = EbpfLoader::new()
    ///     .set_max_entries("map", 64)
    ///     .load_file("file.o")?;
    /// # Ok::<(), aya::EbpfError>(())
    /// ```
    ///
    pub fn set_max_entries(&mut self, name: &'a str, size: u32) -> &mut Self {
        self.max_entries.insert(name, size);
        self
    }

    /// Treat the provided program as an [`Extension`]
    ///
    /// When attempting to load the program with the provided `name`
    /// the program type is forced to be ] [`Extension`] and is not
    /// inferred from the ELF section name.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use aya::EbpfLoader;
    ///
    /// let bpf = EbpfLoader::new()
    ///     .extension("myfunc")
    ///     .load_file("file.o")?;
    /// # Ok::<(), aya::EbpfError>(())
    /// ```
    ///
    pub fn extension(&mut self, name: &'a str) -> &mut Self {
        self.extensions.insert(name);
        self
    }

    /// Sets BPF verifier log level.
    ///
    /// # Example
    ///
    /// ```no_run
    /// use aya::{EbpfLoader, VerifierLogLevel};
    ///
    /// let bpf = EbpfLoader::new()
    ///     .verifier_log_level(VerifierLogLevel::VERBOSE | VerifierLogLevel::STATS)
    ///     .load_file("file.o")?;
    /// # Ok::<(), aya::EbpfError>(())
    /// ```
    ///
    pub fn verifier_log_level(&mut self, level: VerifierLogLevel) -> &mut Self {
        self.verifier_log_level = level;
        self
    }

    /// Loads eBPF bytecode from a file.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use aya::EbpfLoader;
    ///
    /// let bpf = EbpfLoader::new().load_file("file.o")?;
    /// # Ok::<(), aya::EbpfError>(())
    /// ```
    pub fn load_file<P: AsRef<Path>>(&mut self, path: P) -> Result<Ebpf, EbpfError> {
        let path = path.as_ref();
        self.load(&fs::read(path).map_err(|error| EbpfError::FileError {
            path: path.to_owned(),
            error,
        })?)
    }

    /// Loads eBPF bytecode from a buffer.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use aya::EbpfLoader;
    /// use std::fs;
    ///
    /// let data = fs::read("file.o").unwrap();
    /// let bpf = EbpfLoader::new().load(&data)?;
    /// # Ok::<(), aya::EbpfError>(())
    /// ```
    pub fn load(&mut self, data: &[u8]) -> Result<Ebpf, EbpfError> {
        let Self {
            btf,
            map_pin_path: _,
            globals,
            max_entries,
            extensions,
            verifier_log_level,
            allow_unsupported_maps,
        } = self;
        let mut obj = Object::parse(data)?;
        obj.patch_map_data(globals.clone())?;
        let btf_fd = if let Some(features) = &FEATURES.btf() {
            if let Some(btf) = obj.fixup_and_sanitize_btf(features)? {
                match load_btf(btf.to_bytes(), *verifier_log_level) {
                    Ok(btf_fd) => Some(Arc::new(btf_fd)),
                    // Only report an error here if the BTF is truly needed, otherwise proceed without.
                    Err(err) => {
                        for program in obj.programs.values() {
                            match program.section {
                                ProgramSection::Extension
                                | ProgramSection::FEntry { sleepable: _ }
                                | ProgramSection::FExit { sleepable: _ }
                                | ProgramSection::Lsm { sleepable: _ }
                                | ProgramSection::BtfTracePoint => {
                                    return Err(EbpfError::BtfError(err))
                                }
                                ProgramSection::KRetProbe
                                | ProgramSection::KProbe
                                | ProgramSection::UProbe { sleepable: _ }
                                | ProgramSection::URetProbe { sleepable: _ }
                                | ProgramSection::TracePoint
                                | ProgramSection::SocketFilter
                                | ProgramSection::Xdp {
                                    frags: _,
                                    attach_type: _,
                                }
                                | ProgramSection::SkMsg
                                | ProgramSection::SkSkbStreamParser
                                | ProgramSection::SkSkbStreamVerdict
                                | ProgramSection::SockOps
                                | ProgramSection::SchedClassifier
                                | ProgramSection::CgroupSkb
                                | ProgramSection::CgroupSkbIngress
                                | ProgramSection::CgroupSkbEgress
                                | ProgramSection::CgroupSockAddr { attach_type: _ }
                                | ProgramSection::CgroupSysctl
                                | ProgramSection::CgroupSockopt { attach_type: _ }
                                | ProgramSection::LircMode2
                                | ProgramSection::PerfEvent
                                | ProgramSection::RawTracePoint
                                | ProgramSection::SkLookup
                                | ProgramSection::CgroupSock { attach_type: _ }
                                | ProgramSection::CgroupDevice => {}
                            }
                        }
                        warn!("Object BTF couldn't be loaded in the kernel: {err}");
                        None
                    }
                }
            } else {
                None
            }
        } else {
            warn!("BTF is not supported in the kernel");
            None
        };

        if let Some(btf) = &btf {
            obj.relocate_btf(btf)?;
        }
        let mut maps = HashMap::new();

        for (name, mut obj) in obj.maps.drain() {
            if let (false, EbpfSectionKind::Bss | EbpfSectionKind::Data | EbpfSectionKind::Rodata) =
                (FEATURES.bpf_global_data(), obj.section_kind())
            {
                continue;
            }
            let num_cpus = || -> Result<u32, EbpfError> {
                Ok(possible_cpus()
                    .map_err(|error| EbpfError::FileError {
                        path: PathBuf::from(POSSIBLE_CPUS),
                        error,
                    })?
                    .len() as u32)
            };
            let map_type: bpf_map_type = obj.map_type().try_into().map_err(MapError::from)?;

            // if user provided a max_entries override, use that, otherwise use the value from the object
            if let Some(max_entries) = max_entries_override(
                map_type,
                max_entries.get(name.as_str()).copied(),
                || obj.max_entries(),
                num_cpus,
                || page_size() as u32,
            )? {
                debug!("Overriding max_entries for map {name} to {max_entries}");
                obj.set_max_entries(max_entries)
            }
            match obj.map_type().try_into() {
                Ok(BPF_MAP_TYPE_CPUMAP) => {
                    obj.set_value_size(if FEATURES.cpumap_prog_id() { 8 } else { 4 })
                }
                Ok(BPF_MAP_TYPE_DEVMAP | BPF_MAP_TYPE_DEVMAP_HASH) => {
                    obj.set_value_size(if FEATURES.devmap_prog_id() { 8 } else { 4 })
                }
                _ => (),
            }
            let btf_fd = btf_fd.as_deref().map(|fd| fd.as_fd());
            let mut map = match obj.pinning() {
                PinningType::None => MapData::create(obj, &name, btf_fd)?,
                PinningType::ByName => {
                    // pin maps in /sys/fs/bpf by default to align with libbpf
                    // behavior https://github.com/libbpf/libbpf/blob/v1.2.2/src/libbpf.c#L2161.
                    // let path = map_pin_path
                    //     .as_deref()
                    //     .unwrap_or_else(|| Path::new("/sys/fs/bpf"));
                    //
                    // MapData::create_pinned_by_name(path, obj, &name, btf_fd)?
                    unimplemented!(
                        "pin maps in /sys/fs/bpf by default to align with libbpf behavior"
                    );
                }
            };
            map.finalize()?;
            maps.insert(name, map);
        }
        let text_sections = obj
            .functions
            .keys()
            .map(|(section_index, _)| *section_index)
            .collect();

        maps.iter()
            .map(|(s, data)| (s.as_str(), data.fd().as_fd().as_raw_fd(), data.obj()))
            .for_each(|(s, fd, obj)| {
                let x = obj.section_index();
                info!("section {s} fd {fd} section_index {x}");
            });

        obj.relocate_maps(
            maps.iter()
                .map(|(s, data)| (s.as_str(), data.fd().as_fd().as_raw_fd() as _, data.obj())),
            &text_sections,
        )?;

        obj.relocate_calls(&text_sections)?;
        obj.sanitize_functions(&FEATURES);

        let programs = obj
            .programs
            .drain()
            .map(|(name, prog_obj)| {
                let function_obj = obj.functions.get(&prog_obj.function_key()).unwrap().clone();

                let prog_name = if FEATURES.bpf_name() {
                    Some(name.clone())
                } else {
                    None
                };
                let section = prog_obj.section.clone();
                let obj = (prog_obj, function_obj);

                let btf_fd = btf_fd.clone();
                let program = if extensions.contains(name.as_str()) {
                    Program::Extension(Extension {
                        data: ProgramData::new(prog_name, obj, btf_fd, *verifier_log_level),
                    })
                } else {
                    match &section {
                        ProgramSection::KProbe => Program::KProbe(KProbe {
                            data: ProgramData::new(prog_name, obj, btf_fd, *verifier_log_level),
                            kind: ProbeKind::KProbe,
                        }),
                        _ => {
                            unimplemented!()
                        }
                    }
                };
                (name, program)
            })
            .collect();

        let maps = maps
            .drain()
            .map(parse_map)
            .collect::<Result<HashMap<String, Map>, EbpfError>>()?;
        if !*allow_unsupported_maps {
            maps.iter().try_for_each(|(_, x)| match x {
                Map::Unsupported(map) => Err(EbpfError::MapError(MapError::Unsupported {
                    map_type: map.obj().map_type(),
                })),
                _ => Ok(()),
            })?;
        };
        Ok(Ebpf { maps, programs })
    }
}

fn parse_map(data: (String, MapData)) -> Result<(String, Map), EbpfError> {
    let (name, map) = data;
    let map_type = bpf_map_type::try_from(map.obj().map_type()).map_err(MapError::from)?;
    let map = match map_type {
        BPF_MAP_TYPE_ARRAY => Map::Array(map),
        BPF_MAP_TYPE_PERCPU_ARRAY => Map::PerCpuArray(map),
        BPF_MAP_TYPE_PROG_ARRAY => Map::ProgramArray(map),
        BPF_MAP_TYPE_HASH => Map::HashMap(map),
        BPF_MAP_TYPE_LRU_HASH => Map::LruHashMap(map),
        BPF_MAP_TYPE_PERCPU_HASH => Map::PerCpuHashMap(map),
        BPF_MAP_TYPE_LRU_PERCPU_HASH => Map::PerCpuLruHashMap(map),
        BPF_MAP_TYPE_PERF_EVENT_ARRAY => Map::PerfEventArray(map),
        BPF_MAP_TYPE_RINGBUF => Map::RingBuf(map),
        BPF_MAP_TYPE_SOCKHASH => Map::SockHash(map),
        BPF_MAP_TYPE_SOCKMAP => Map::SockMap(map),
        BPF_MAP_TYPE_BLOOM_FILTER => Map::BloomFilter(map),
        BPF_MAP_TYPE_LPM_TRIE => Map::LpmTrie(map),
        BPF_MAP_TYPE_STACK => Map::Stack(map),
        BPF_MAP_TYPE_STACK_TRACE => Map::StackTraceMap(map),
        BPF_MAP_TYPE_QUEUE => Map::Queue(map),
        BPF_MAP_TYPE_CPUMAP => Map::CpuMap(map),
        BPF_MAP_TYPE_DEVMAP => Map::DevMap(map),
        BPF_MAP_TYPE_DEVMAP_HASH => Map::DevMapHash(map),
        BPF_MAP_TYPE_XSKMAP => Map::XskMap(map),
        m => {
            warn!("The map {name} is of type {:#?} which is currently unsupported in Aya, use `allow_unsupported_maps()` to load it anyways", m);
            Map::Unsupported(map)
        }
    };

    Ok((name, map))
}

/// Computes the value which should be used to override the max_entries value of the map
/// based on the user-provided override and the rules for that map type.
fn max_entries_override(
    map_type: bpf_map_type,
    user_override: Option<u32>,
    current_value: impl Fn() -> u32,
    num_cpus: impl Fn() -> Result<u32, EbpfError>,
    page_size: impl Fn() -> u32,
) -> Result<Option<u32>, EbpfError> {
    let max_entries = || user_override.unwrap_or_else(&current_value);
    Ok(match map_type {
        BPF_MAP_TYPE_PERF_EVENT_ARRAY if max_entries() == 0 => Some(num_cpus()?),
        BPF_MAP_TYPE_RINGBUF => Some(adjust_to_page_size(max_entries(), page_size()))
            .filter(|adjusted| *adjusted != max_entries())
            .or(user_override),
        _ => user_override,
    })
}

// Adjusts the byte size of a RingBuf map to match a power-of-two multiple of the page size.
//
// This mirrors the logic used by libbpf.
// See https://github.com/libbpf/libbpf/blob/ec6f716eda43/src/libbpf.c#L2461-L2463
fn adjust_to_page_size(byte_size: u32, page_size: u32) -> u32 {
    // If the byte_size is zero, return zero and let the verifier reject the map
    // when it is loaded. This is the behavior of libbpf.
    if byte_size == 0 {
        return 0;
    }
    // TODO: Replace with primitive method when int_roundings (https://github.com/rust-lang/rust/issues/88581)
    // is stabilized.
    fn div_ceil(n: u32, rhs: u32) -> u32 {
        let d = n / rhs;
        let r = n % rhs;
        if r > 0 && rhs > 0 {
            d + 1
        } else {
            d
        }
    }
    let pages_needed = div_ceil(byte_size, page_size);
    page_size * pages_needed.next_power_of_two()
}

/// Try loading the BTF data into the kernel.
///
/// The kernel will write error messages to the provided logger. User should provide enough capacity
/// to store the error messages.
fn load_btf(raw_btf: Vec<u8>, verifier_log_level: VerifierLogLevel) -> Result<OwnedFd, BtfError> {
    let (ret, verifier_log) = retry_with_verifier_logs(10, |logger| {
        bpf_load_btf(raw_btf.as_slice(), logger, verifier_log_level)
    });
    ret.map_err(|(_, io_error)| BtfError::LoadError {
        io_error,
        verifier_log,
    })
}
/// The main entry point into the library, used to work with eBPF programs and maps.
#[derive(Debug)]
pub struct Ebpf {
    maps: HashMap<String, Map>,
    programs: HashMap<String, Program>,
}

impl Ebpf {
    /// Loads eBPF bytecode from a file.
    ///
    /// Parses the given object code file and initializes the [maps](crate::maps) defined in it. If
    /// the kernel supports [BTF](Btf) debug info, it is automatically loaded from
    /// `/sys/kernel/btf/vmlinux`.
    ///
    /// For more loading options, see [EbpfLoader].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use aya::Ebpf;
    ///
    /// let bpf = Ebpf::load_file("file.o")?;
    /// # Ok::<(), aya::EbpfError>(())
    /// ```
    pub fn load_file<P: AsRef<str>>(path: P) -> Result<Self, EbpfError> {
        // EbpfLoader::new()
        // .btf(Btf::from_sys_fs().ok().as_ref())
        // .load_file(path)
        unimplemented!()
    }

    /// Loads eBPF bytecode from a buffer.
    ///
    /// Parses the object code contained in `data` and initializes the
    /// [maps](crate::maps) defined in it. If the kernel supports [BTF](Btf)
    /// debug info, it is automatically loaded from `/sys/kernel/btf/vmlinux`.
    ///
    /// For more loading options, see [EbpfLoader].
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use aya::{Ebpf, Btf};
    /// use std::fs;
    ///
    /// let data = fs::read("file.o").unwrap();
    /// // load the BTF data from /sys/kernel/btf/vmlinux
    /// let bpf = Ebpf::load(&data)?;
    /// # Ok::<(), aya::EbpfError>(())
    /// ```
    pub fn load(data: &[u8]) -> Result<Self, EbpfError> {
        EbpfLoader::new()
            // .btf(Btf::from_sys_fs().ok().as_ref())
            .load(data)
    }
    /// Returns a reference to the map with the given name.
    ///
    /// The returned type is mostly opaque. In order to do anything useful with it you need to
    /// convert it to a [typed map](crate::maps).
    ///
    /// For more details and examples on maps and their usage, see the [maps module
    /// documentation][crate::maps].
    pub fn map(&self, name: &str) -> Option<&Map> {
        self.maps.get(name)
    }

    /// Returns a mutable reference to the map with the given name.
    ///
    /// The returned type is mostly opaque. In order to do anything useful with it you need to
    /// convert it to a [typed map](crate::maps).
    ///
    /// For more details and examples on maps and their usage, see the [maps module
    /// documentation][crate::maps].
    pub fn map_mut(&mut self, name: &str) -> Option<&mut Map> {
        self.maps.get_mut(name)
    }

    /// Takes ownership of a map with the given name.
    ///
    /// Use this when borrowing with [`map`](crate::Ebpf::map) or [`map_mut`](crate::Ebpf::map_mut)
    /// is not possible (eg when using the map from an async task). The returned
    /// map will be closed on `Drop`, therefore the caller is responsible for
    /// managing its lifetime.
    ///
    /// The returned type is mostly opaque. In order to do anything useful with it you need to
    /// convert it to a [typed map](crate::maps).
    ///
    /// For more details and examples on maps and their usage, see the [maps module
    /// documentation][crate::maps].
    pub fn take_map(&mut self, name: &str) -> Option<Map> {
        self.maps.remove(name)
    }

    /// An iterator over all the maps.
    ///
    /// # Examples
    /// ```no_run
    /// # let mut bpf = aya::Ebpf::load(&[])?;
    /// for (name, map) in bpf.maps() {
    ///     println!(
    ///         "found map `{}`",
    ///         name,
    ///     );
    /// }
    /// # Ok::<(), aya::EbpfError>(())
    /// ```
    pub fn maps(&self) -> impl Iterator<Item = (&str, &Map)> {
        self.maps.iter().map(|(name, map)| (name.as_str(), map))
    }

    /// A mutable iterator over all the maps.
    ///
    /// # Examples
    /// ```no_run
    /// # use std::path::Path;
    /// # #[derive(thiserror::Error, Debug)]
    /// # enum Error {
    /// #     #[error(transparent)]
    /// #     Ebpf(#[from] aya::EbpfError),
    /// #     #[error(transparent)]
    /// #     Pin(#[from] aya::pin::PinError)
    /// # }
    /// # let mut bpf = aya::Ebpf::load(&[])?;
    /// # let pin_path = Path::new("/tmp/pin_path");
    /// for (_, map) in bpf.maps_mut() {
    ///     map.pin(pin_path)?;
    /// }
    /// # Ok::<(), Error>(())
    /// ```
    pub fn maps_mut(&mut self) -> impl Iterator<Item = (&str, &mut Map)> {
        self.maps.iter_mut().map(|(name, map)| (name.as_str(), map))
    }
    /// Returns a reference to the program with the given name.
    ///
    /// You can use this to inspect a program and its properties. To load and attach a program, use
    /// [program_mut](Self::program_mut) instead.
    ///
    /// For more details on programs and their usage, see the [programs module
    /// documentation](crate::programs).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # let bpf = aya::Ebpf::load(&[])?;
    /// let program = bpf.program("SSL_read").unwrap();
    /// println!("program SSL_read is of type {:?}", program.prog_type());
    /// # Ok::<(), aya::EbpfError>(())
    /// ```
    pub fn program(&self, name: &str) -> Option<&Program> {
        self.programs.get(name)
    }

    /// Returns a mutable reference to the program with the given name.
    ///
    /// Used to get a program before loading and attaching it. For more details on programs and
    /// their usage, see the [programs module documentation](crate::programs).
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # let mut bpf = aya::Ebpf::load(&[])?;
    /// use aya::programs::UProbe;
    ///
    /// let program: &mut UProbe = bpf.program_mut("SSL_read").unwrap().try_into()?;
    /// program.load()?;
    /// program.attach(Some("SSL_read"), 0, "libssl", None)?;
    /// # Ok::<(), aya::EbpfError>(())
    /// ```
    pub fn program_mut(&mut self, name: &str) -> Option<&mut Program> {
        self.programs.get_mut(name)
    }

    /// An iterator over all the programs.
    ///
    /// # Examples
    /// ```no_run
    /// # let bpf = aya::Ebpf::load(&[])?;
    /// for (name, program) in bpf.programs() {
    ///     println!(
    ///         "found program `{}` of type `{:?}`",
    ///         name,
    ///         program.prog_type()
    ///     );
    /// }
    /// # Ok::<(), aya::EbpfError>(())
    /// ```
    pub fn programs(&self) -> impl Iterator<Item = (&str, &Program)> {
        self.programs.iter().map(|(s, p)| (s.as_str(), p))
    }

    /// An iterator mutably referencing all of the programs.
    ///
    /// # Examples
    /// ```no_run
    /// # use std::path::Path;
    /// # #[derive(thiserror::Error, Debug)]
    /// # enum Error {
    /// #     #[error(transparent)]
    /// #     Ebpf(#[from] aya::EbpfError),
    /// #     #[error(transparent)]
    /// #     Pin(#[from] aya::pin::PinError)
    /// # }
    /// # let mut bpf = aya::Ebpf::load(&[])?;
    /// # let pin_path = Path::new("/tmp/pin_path");
    /// for (_, program) in bpf.programs_mut() {
    ///     program.pin(pin_path)?;
    /// }
    /// # Ok::<(), Error>(())
    /// ```
    pub fn programs_mut(&mut self) -> impl Iterator<Item = (&str, &mut Program)> {
        self.programs.iter_mut().map(|(s, p)| (s.as_str(), p))
    }
}

/// The error type returned by [`Ebpf::load_file`] and [`Ebpf::load`].
#[derive(Debug, Error)]
pub enum EbpfError {
    /// Error loading file
    #[error("error loading {path}")]
    FileError {
        /// The file path
        path: PathBuf,
        #[source]
        /// The original io::Error
        error: io::Error,
    },

    /// Unexpected pinning type
    #[error("unexpected pinning type {name}")]
    UnexpectedPinningType {
        /// The value encountered
        name: u32,
    },

    /// Error parsing BPF object
    #[error("error parsing BPF object: {0}")]
    ParseError(#[from] ParseError),

    /// Error parsing BTF object
    #[error("BTF error: {0}")]
    BtfError(#[from] BtfError),

    /// Error performing relocations
    #[error("error relocating function")]
    RelocationError(#[from] EbpfRelocationError),

    /// Error performing relocations
    #[error("error relocating section")]
    BtfRelocationError(#[from] BtfRelocationError),

    /// No BTF parsed for object
    #[error("no BTF parsed for object")]
    NoBTF,

    #[error("map error: {0}")]
    /// A map error
    MapError(#[from] MapError),

    #[error("program error: {0}")]
    /// A program error
    ProgramError(#[from] ProgramError),
}

/// Marker trait for types that can safely be converted to and from byte slices.
pub unsafe trait Pod: Copy + 'static {}

macro_rules! unsafe_impl_pod {
    ($($struct_name:ident),+ $(,)?) => {
        $(
            unsafe impl Pod for $struct_name { }
        )+
    }
}

unsafe_impl_pod!(i8, u8, i16, u16, i32, u32, i64, u64, u128, i128);

// It only makes sense that an array of POD types is itself POD
unsafe impl<T: Pod, const N: usize> Pod for [T; N] {}
/// Global data that can be exported to eBPF programs before they are loaded.
///
/// Valid global data includes `Pod` types and slices of `Pod` types. See also
/// [EbpfLoader::set_global].
pub struct GlobalData<'a> {
    bytes: &'a [u8],
}

impl<'a, T: Pod> From<&'a [T]> for GlobalData<'a> {
    fn from(s: &'a [T]) -> Self {
        GlobalData {
            bytes: bytes_of_slice(s),
        }
    }
}

impl<'a, T: Pod> From<&'a T> for GlobalData<'a> {
    fn from(v: &'a T) -> Self {
        GlobalData {
            // Safety: v is Pod
            bytes: unsafe { bytes_of(v) },
        }
    }
}

pub(crate) fn page_size() -> usize {
    // Safety: libc
    4096
}

// bytes_of converts a <T> to a byte slice
pub(crate) unsafe fn bytes_of<T: Pod>(val: &T) -> &[u8] {
    let size = mem::size_of::<T>();
    slice::from_raw_parts(slice::from_ref(val).as_ptr().cast(), size)
}

pub(crate) fn bytes_of_slice<T: Pod>(val: &[T]) -> &[u8] {
    let size = val.len().wrapping_mul(mem::size_of::<T>());
    // Safety:
    // Any alignment is allowed.
    // The size is determined in this function.
    // The Pod trait ensures the type is valid to cast to bytes.
    unsafe { slice::from_raw_parts(val.as_ptr().cast(), size) }
}
