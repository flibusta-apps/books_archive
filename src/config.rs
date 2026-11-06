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
    pub inpx_limits: crate::services::inpx::Limits,
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
        let api_key = authentication_key(
            std::env::var("API_KEY").ok(),
            std::env::var("ALLOW_UNAUTHENTICATED").ok().as_deref(),
        )
        .unwrap_or_else(|message| panic!("{message}"));
        if api_key.is_none() {
            tracing::warn!("SECURITY WARNING: ALLOW_UNAUTHENTICATED=true; all book downloads are PUBLIC without authentication");
        }
        Config {
            archive_dir: PathBuf::from(get_env("ARCHIVE_DIR")),
            api_key,
            inpx_path: std::env::var("INPX_PATH")
                .ok()
                .filter(|p| !p.is_empty())
                .map(PathBuf::from),
            port: parse_env("PORT", 8080u16),
            max_concurrent_downloads: parse_env("MAX_CONCURRENT_DOWNLOADS", 16usize),
            max_entry_bytes: parse_env("MAX_ENTRY_BYTES", 512u64 * 1024 * 1024),
            request_timeout: Duration::from_secs(parse_env("REQUEST_TIMEOUT_SECS", 600u64)),
            inpx_limits: crate::services::inpx::Limits {
                max_decompressed_bytes: parse_env(
                    "INPX_MAX_DECOMPRESSED_BYTES",
                    2u64 * 1024 * 1024 * 1024,
                ),
                max_records: parse_env("INPX_MAX_RECORDS", 20_000_000u64),
            },
        }
    }
}

fn authentication_key(
    key: Option<String>,
    allow: Option<&str>,
) -> Result<Option<String>, &'static str> {
    match key.filter(|key| !key.trim().is_empty()) {
        Some(key) => Ok(Some(key)),
        None if allow == Some("true") => Ok(None),
        None => Err("API_KEY must be set and non-empty; to explicitly allow public access set ALLOW_UNAUTHENTICATED=true"),
    }
}

pub static CONFIG: Lazy<Config> = Lazy::new(Config::load);

#[cfg(test)]
mod tests {
    use super::authentication_key;

    #[test]
    fn authentication_is_required_unless_explicitly_disabled() {
        for key in [None, Some(String::new()), Some("  ".into())] {
            for allow in [None, Some("false"), Some("TRUE"), Some("")] {
                assert!(authentication_key(key.clone(), allow).is_err());
            }
            assert_eq!(authentication_key(key, Some("true")), Ok(None));
        }
        for allow in [None, Some("true")] {
            assert_eq!(
                authentication_key(Some("secret".into()), allow),
                Ok(Some("secret".into()))
            );
        }
    }
}
