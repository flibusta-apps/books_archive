use std::{path::PathBuf, str::FromStr, time::Duration};

use once_cell::sync::Lazy;

pub struct Config {
    pub archive_dir: PathBuf,
    pub api_key: Option<String>,
    /// Explicit `.inpx` path (`INPX_PATH`); discovered in `archive_dir` when unset.
    pub inpx_path: Option<PathBuf>,
    pub port: u16,
    pub max_concurrent_downloads: usize,
    pub max_entry_bytes: u64,
    pub request_timeout: Duration,
}

fn get_env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("Env variable {name} not set!"))
}

/// Parses an optional numeric env var; set-but-invalid values are fatal.
fn parse_env<T: FromStr + PartialOrd + From<u8>>(name: &str, default: T) -> T {
    match std::env::var(name) {
        Err(_) => default,
        Ok(v) => match v.trim().parse::<T>() {
            Ok(n) if n >= T::from(1) => n,
            _ => panic!("Env variable {name} must be a positive number, got {v:?}"),
        },
    }
}

impl Config {
    pub fn load() -> Config {
        Config {
            archive_dir: PathBuf::from(get_env("ARCHIVE_DIR")),
            api_key: std::env::var("API_KEY").ok().filter(|k| !k.is_empty()),
            inpx_path: std::env::var("INPX_PATH")
                .ok()
                .filter(|p| !p.is_empty())
                .map(PathBuf::from),
            port: parse_env("PORT", 8080u16),
            max_concurrent_downloads: parse_env("MAX_CONCURRENT_DOWNLOADS", 16usize),
            max_entry_bytes: parse_env("MAX_ENTRY_BYTES", 512u64 * 1024 * 1024),
            request_timeout: Duration::from_secs(parse_env("REQUEST_TIMEOUT_SECS", 600u64)),
        }
    }
}

pub static CONFIG: Lazy<Config> = Lazy::new(Config::load);
