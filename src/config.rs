use std::path::PathBuf;

use once_cell::sync::Lazy;

pub struct Config {
    pub archive_dir: PathBuf,
    pub api_key: Option<String>,
    /// Explicit `.inpx` path (`INPX_PATH`); discovered in `archive_dir` when unset.
    pub inpx_path: Option<PathBuf>,
}

fn get_env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("Env variable {name} not set!"))
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
        }
    }
}

pub static CONFIG: Lazy<Config> = Lazy::new(Config::load);
