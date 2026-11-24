use std::ffi::OsString;
use std::fmt;
use std::io;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::Url;
use serde::Deserialize;

use crate::js::Limits;

const APP_DIR: &str = "sistem-jedan";
const CONFIG_FILE: &str = "config.toml";
const DEFAULT_TIMEOUT_SECONDS: u64 = 5;
const MAX_TIMEOUT_SECONDS: u64 = 3600;
const DEFAULT_MEMORY_LIMIT_MB: u64 = 64;
const DEFAULT_MEMORY_LIMIT_BYTES: usize = 64 * 1024 * 1024;
const DEFAULT_MAX_CONCURRENT_REQUESTS: u32 = 8;
const DEFAULT_MAX_CALLS_PER_EXECUTION: u32 = 32;
const BYTES_PER_MB: u64 = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("missing value for --config")]
    MissingConfigValue,
    #[error("--config given more than once")]
    DuplicateConfig,
    #[error("unexpected argument: {}", .0.to_string_lossy())]
    UnexpectedArgument(OsString),
    #[error("failed to read {}: {source}", .path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("invalid configuration in {}{}: {message}", .path.display(), .line.map(|line| format!(" at line {line}")).unwrap_or_default())]
    Parse {
        path: PathBuf,
        line: Option<usize>,
        message: String,
    },
    #[error(
        "provider.endpoint must be an https URL with a host and no embedded credentials, query string or fragment"
    )]
    InvalidEndpoint,
    #[error("provider.model must not be empty")]
    EmptyModel,
    #[error("provider.api_key must not be empty")]
    EmptyApiKey,
    #[error("execution.timeout_seconds must be between 1 and {MAX_TIMEOUT_SECONDS}")]
    InvalidTimeout,
    #[error("execution.memory_limit_mb must be at least 1 and fit in addressable memory")]
    InvalidMemoryLimit,
    #[error("execution.max_concurrent_requests must be at least 1")]
    InvalidMaxConcurrentRequests,
    #[error("execution.max_calls_per_execution must be at least 1")]
    InvalidMaxCallsPerExecution,
}

pub struct ApiKey(String);

impl ApiKey {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApiKey(<redacted>)")
    }
}

#[derive(Debug)]
pub struct Provider {
    pub endpoint: Url,
    pub model: String,
    pub api_key: ApiKey,
}

#[derive(Debug, Clone, Copy)]
pub struct ExecutionSettings {
    pub limits: Limits,
    pub max_concurrent_requests: NonZeroU32,
    pub max_calls_per_execution: NonZeroU32,
}

impl ExecutionSettings {
    pub const DEFAULT: Self = Self {
        limits: Limits {
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECONDS),
            memory_limit: DEFAULT_MEMORY_LIMIT_BYTES,
        },
        max_concurrent_requests: NonZeroU32::new(DEFAULT_MAX_CONCURRENT_REQUESTS).unwrap(),
        max_calls_per_execution: NonZeroU32::new(DEFAULT_MAX_CALLS_PER_EXECUTION).unwrap(),
    };
}

#[derive(Debug)]
pub struct ConfigFile {
    pub path: PathBuf,
    pub provider: Provider,
}

#[derive(Debug)]
pub struct Config {
    pub file: Option<ConfigFile>,
    pub execution: ExecutionSettings,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    provider: RawProvider,
    #[serde(default)]
    execution: RawExecution,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProvider {
    endpoint: String,
    model: String,
    api_key: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, default)]
struct RawExecution {
    timeout_seconds: u64,
    memory_limit_mb: u64,
    max_concurrent_requests: u32,
    max_calls_per_execution: u32,
}

impl Default for RawExecution {
    fn default() -> Self {
        Self {
            timeout_seconds: DEFAULT_TIMEOUT_SECONDS,
            memory_limit_mb: DEFAULT_MEMORY_LIMIT_MB,
            max_concurrent_requests: DEFAULT_MAX_CONCURRENT_REQUESTS,
            max_calls_per_execution: DEFAULT_MAX_CALLS_PER_EXECUTION,
        }
    }
}

impl RawExecution {
    fn validate(self) -> Result<ExecutionSettings, ConfigError> {
        if !(1..=MAX_TIMEOUT_SECONDS).contains(&self.timeout_seconds) {
            return Err(ConfigError::InvalidTimeout);
        }
        let memory_limit = self
            .memory_limit_mb
            .checked_mul(BYTES_PER_MB)
            .filter(|&bytes| bytes > 0)
            .and_then(|bytes| usize::try_from(bytes).ok())
            .ok_or(ConfigError::InvalidMemoryLimit)?;
        Ok(ExecutionSettings {
            limits: Limits {
                timeout: Duration::from_secs(self.timeout_seconds),
                memory_limit,
            },
            max_concurrent_requests: NonZeroU32::new(self.max_concurrent_requests)
                .ok_or(ConfigError::InvalidMaxConcurrentRequests)?,
            max_calls_per_execution: NonZeroU32::new(self.max_calls_per_execution)
                .ok_or(ConfigError::InvalidMaxCallsPerExecution)?,
        })
    }
}

impl RawProvider {
    fn validate(self) -> Result<Provider, ConfigError> {
        let endpoint = parse_endpoint(&self.endpoint).ok_or(ConfigError::InvalidEndpoint)?;
        if self.model.trim().is_empty() {
            return Err(ConfigError::EmptyModel);
        }
        if self.api_key.trim().is_empty() {
            return Err(ConfigError::EmptyApiKey);
        }
        Ok(Provider {
            endpoint,
            model: self.model,
            api_key: ApiKey(self.api_key),
        })
    }
}

fn parse_endpoint(endpoint: &str) -> Option<Url> {
    let rest = endpoint.strip_prefix("https://")?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty()
        || authority.contains('@')
        || endpoint
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
    {
        return None;
    }
    Url::parse(endpoint).ok().filter(|url| {
        url.scheme() == "https"
            && url.host_str().is_some_and(|host| !host.is_empty())
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
    })
}

pub fn load(args: impl IntoIterator<Item = OsString>) -> Result<Config, ConfigError> {
    match parse_args(args)? {
        Some(path) => match read(&path)? {
            Some(source) => parse(path, &source),
            None => Err(ConfigError::Read {
                path,
                source: io::ErrorKind::NotFound.into(),
            }),
        },
        None => match default_path() {
            Some(path) => match read(&path)? {
                Some(source) => parse(path, &source),
                None => Ok(defaults()),
            },
            None => Ok(defaults()),
        },
    }
}

fn defaults() -> Config {
    Config {
        file: None,
        execution: ExecutionSettings::DEFAULT,
    }
}

fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<Option<PathBuf>, ConfigError> {
    let mut args = args.into_iter();
    let mut path = None;
    while let Some(arg) = args.next() {
        let value = if arg == "--config" {
            args.next().ok_or(ConfigError::MissingConfigValue)?
        } else if let Some(value) = arg.to_str().and_then(|arg| arg.strip_prefix("--config=")) {
            OsString::from(value)
        } else {
            return Err(ConfigError::UnexpectedArgument(arg));
        };
        if value.is_empty() {
            return Err(ConfigError::MissingConfigValue);
        }
        if path.replace(PathBuf::from(value)).is_some() {
            return Err(ConfigError::DuplicateConfig);
        }
    }
    Ok(path)
}

fn default_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .filter(|path| path.is_absolute())
                .map(|home| home.join(".config"))
        })?;
    Some(base.join(APP_DIR).join(CONFIG_FILE))
}

fn read(path: &Path) -> Result<Option<String>, ConfigError> {
    match std::fs::read_to_string(path) {
        Ok(source) => Ok(Some(source)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ConfigError::Read {
            path: path.to_owned(),
            source,
        }),
    }
}

fn parse(path: PathBuf, source: &str) -> Result<Config, ConfigError> {
    let raw: RawConfig = toml::from_str(source).map_err(|error| ConfigError::Parse {
        line: error
            .span()
            .map(|span| source[..span.start].matches('\n').count() + 1),
        message: error.message().to_owned(),
        path: path.clone(),
    })?;
    Ok(Config {
        file: Some(ConfigFile {
            path,
            provider: raw.provider.validate()?,
        }),
        execution: raw.execution.validate()?,
    })
}
