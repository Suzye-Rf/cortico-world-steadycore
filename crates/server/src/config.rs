//! 配置：`data/core.toml`。文件不在就用模板生成一份，改完重启生效。

use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub port: u16,
    /// 端口被别的程序占着时，往上试几个端口（0 = 不试，直接无声模式）
    pub port_scan: u16,
    pub host: String,
    pub mongo_uri: String,
    pub db: String,
    /// tick 基准间隔（秒）。设计文档是 2400（40 分钟），调试时可以调到 30
    pub tick_interval_sec: u64,
    /// tick 抖动（秒），实际间隔 = 基准 ± 抖动 × U(0,1)
    pub tick_jitter_sec: u64,
    pub log_keep_days: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: 7801,
            port_scan: 9,
            host: "127.0.0.1".to_string(),
            mongo_uri: "mongodb://127.0.0.1:27017".to_string(),
            db: "steadycore".to_string(),
            tick_interval_sec: 2400,
            tick_jitter_sec: 900,
            log_keep_days: 14,
        }
    }
}

const TEMPLATE: &str = r#"# steadycore 配置（改完重启生效）
# 端口只绑本机，不对外。
port = 7801
# 被别的程序占着时往上试几个端口，0 = 不试（直接进无声模式）
port_scan = 9
host = "127.0.0.1"

mongo_uri = "mongodb://127.0.0.1:27017"
db = "steadycore"

# tick 基准间隔（秒）= 40 分钟；抖动 ±15 分钟（§10.2 不确定性之一）
tick_interval_sec = 2400
tick_jitter_sec = 900

log_keep_days = 14
"#;

/// 工作根目录：优先 `STEADY_CORE_HOME`，否则是当前目录。
pub fn home() -> PathBuf {
    std::env::var("STEADY_CORE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

pub fn data_dir() -> PathBuf {
    home().join("data")
}

pub fn log_dir() -> PathBuf {
    data_dir().join("logs")
}

pub fn config_path() -> PathBuf {
    data_dir().join("core.toml")
}

impl Config {
    pub fn load_or_create() -> Result<Self> {
        let dir = data_dir();
        fs::create_dir_all(&dir)
            .with_context(|| format!("建不出数据目录 {}", dir.display()))?;
        fs::create_dir_all(log_dir())?;

        let path = config_path();
        if !path.exists() {
            fs::write(&path, TEMPLATE)
                .with_context(|| format!("写不了配置文件 {}", path.display()))?;
        }
        let text = fs::read_to_string(&path)?;
        let cfg: Config = toml::from_str(&text)
            .with_context(|| format!("{} 解析失败", path.display()))?;
        Ok(cfg)
    }

    pub fn addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }
}

/// 备份目录用的时间戳：`20261102-161000`。本地时间，人能直接读。
pub fn stamp() -> String {
    chrono::Local::now().format("%Y%m%d-%H%M%S").to_string()
}

/// 毫秒时间戳 → 本地可读时间，没法转就返回原数字。
pub fn fmt_ms(ms: i64) -> String {
    use chrono::{Local, TimeZone};
    Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| ms.to_string())
}
