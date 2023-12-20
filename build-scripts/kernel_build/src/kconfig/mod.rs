use std::{fs, io::Write, path::PathBuf};

use toml::Value;

use crate::utils::cargo_handler::CargoHandler;

/// 内核编译配置的构建器
pub struct KConfigBuilder;

impl KConfigBuilder {
    pub fn build() {
        // 如果存在kernel.config，才去解析
        if fs::metadata("kernel.config").is_ok() {
            // 获取kernel.config所包含的模块
            let modules = ConfigParser::parse_kernel_config();

            // 扫描各模块下以及其包含模块的d.config，然后将所有d.config路径添加到r中
            let mut r = Vec::new();
            for m in modules.iter() {
                if m.enable() {
                    Self::dfs(m, &mut r);
                }
            }

            // 扫描所有d.config以获取features
            let features = ConfigParser::parse_d_configs(&r);

            // 添加feature
            CargoHandler::emit_features(features.as_slice());

            // 生成最终内核编译配置文件D.config
            Self::make_compile_cfg(&features);
        }
    }

    /// 生成最终编译配置文件D.config
    fn make_compile_cfg(features: &Vec<Feature>) {
        let mut cfg_content = String::new();
        for f in features.iter() {
            if f.enable() {
                cfg_content.push_str(&format!("{} = y\n", f.name()));
            } else {
                cfg_content.push_str(&format!("{} = n\n", f.name()));
            }
        }

        let mut file = fs::File::create("D.config").expect("Failed to create file: D.config");
        file.write_all(cfg_content.as_bytes())
            .expect("Failed to write D.config");
    }

    /// 递归找所有模块下的d.config文件路径
    ///
    /// ## 参数
    ///
    /// `module` - 当前模块
    /// `r` - 保存所有d.config文件路径
    /// ## 返回值
    ///
    /// 无
    fn dfs(module: &Module, r: &mut Vec<PathBuf>) {
        println!("{}", module.name());

        let path_str = module.path().as_path().to_str().unwrap().to_string();
        let d_config_str = format!("{}/d.config", path_str);
        let d_config_path = PathBuf::from(&d_config_str);
        let dcfg_content =
            fs::read_to_string(&d_config_path).expect(&format!("Failed to read {}", d_config_str));
        let m_include = ConfigParser::include(&dcfg_content);

        for m in m_include.iter() {
            if m.enable() {
                Self::dfs(m, r);
            }
        }

        r.push(d_config_path);
    }
}

/// 内核编译配置文件解析器
struct ConfigParser;

impl ConfigParser {
    /// 扫描kernel.config获取所包含的模块
    pub fn parse_kernel_config() -> Vec<Module> {
        let cfg_content =
            fs::read_to_string("kernel.config").expect("Failed to read kernel.config.");

        let r = Self::include(&cfg_content);

        return r;
    }

    /// 扫描所有d.config以获取所有feature
    pub fn parse_d_configs(d_configs: &Vec<PathBuf>) -> Vec<Feature> {
        let mut r = Vec::new();
        for d_config in d_configs.iter() {
            r.extend(Self::parse_d_config(d_config));
        }
        return r;
    }

    /// 扫描当前d.config文件获取feature
    pub fn parse_d_config(d_config: &PathBuf) -> Vec<Feature> {
        let path_str = d_config.as_path().to_str().unwrap().to_string();
        let dcfg_content =
            fs::read_to_string(d_config).expect(&format!("Failed to read {}", path_str));
        let dcfg_table: Value =
            toml::from_str(&dcfg_content).expect(&format!("Failed to parse {}", path_str));

        let mut r = Vec::new();
        if let Some(features) = dcfg_table.get("module").unwrap().get("features") {
            for f in features.as_array().unwrap().iter() {
                let name = f.get("name").unwrap().as_str().unwrap().to_string();
                let enable = f.get("enable").unwrap().as_str().unwrap().to_string() == "y";
                r.push(Feature::new(name, enable));
            }
        }
        return r;
    }

    /// 获取所包含的模块
    ///
    /// ## 参数
    ///
    /// `cfg_content` -配置文件内容
    ///
    /// ## 返回值
    ///
    /// 包含的模块集合
    pub fn include(cfg_content: &str) -> Vec<Module> {
        let cfg_table: Value = toml::from_str(&cfg_content).expect("Failed to parse kernel.config");
        let mut r = Vec::new();
        if let Some(include) = cfg_table.get("module").unwrap().get("include") {
            for module in include.as_array().unwrap().iter() {
                let name = module.get("name").unwrap().as_str().unwrap().to_string();
                let path = PathBuf::from(module.get("path").unwrap().as_str().unwrap());
                let enable = module.get("enable").unwrap().as_str().unwrap() == "y";
                r.push(Module::new(name, path, enable));
            }
        }
        return r;
    }
}

/// 模块
struct Module {
    /// 模块名
    name: String,
    /// 模块文件路径
    path: PathBuf,
    /// 是否启用
    enable: bool,
}

impl Module {
    pub fn new(name: String, path: PathBuf, enable: bool) -> Module {
        Module { name, path, enable }
    }

    pub fn name(&self) -> String {
        self.name.clone()
    }

    pub fn path(&self) -> PathBuf {
        self.path.clone()
    }

    pub fn enable(&self) -> bool {
        self.enable.clone()
    }
}

/// feature
pub struct Feature {
    /// feature标签名
    name: String,
    /// 是否启用
    enable: bool,
}

impl Feature {
    pub fn new(name: String, enable: bool) -> Feature {
        Feature { name, enable }
    }

    pub fn name(&self) -> String {
        self.name.clone()
    }

    pub fn enable(&self) -> bool {
        self.enable.clone()
    }
}
