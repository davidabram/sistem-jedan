use std::ffi::OsString;
use std::io;
use std::num::{NonZeroU32, NonZeroUsize};
use std::path::{Path, PathBuf};
use std::time::Duration;

use reqwest::Url;
use serde::Deserialize;
use tokio::sync::Semaphore;

use crate::js::Limits;

const APP_DIR: &str = "sistem-jedan";
const CONFIG_FILE: &str = "config.toml";
const MAX_TIMEOUT_SECONDS: u64 = 3600;
pub(super) const BYTES_PER_MIB: usize = 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub(super) enum ConfigError {
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
    #[error("invalid configuration in {}{}: {message}", .path.display(), .line.map_or_default(|line| format!(" at line {line}")))]
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
    #[error(
        "execution.max_concurrent_requests must be between 1 and {}",
        Semaphore::MAX_PERMITS
    )]
    InvalidMaxConcurrentRequests,
    #[error(
        "execution.max_concurrent_executions must be between 1 and {}",
        Semaphore::MAX_PERMITS
    )]
    InvalidMaxConcurrentExecutions,
    #[error("execution.vcpu_per_execution must be greater than 0 and at most 1")]
    InvalidVcpuPerExecution,
    #[error("execution.max_calls_per_execution must be at least 1")]
    InvalidMaxCallsPerExecution,
}

pub(super) struct Provider {
    pub(super) endpoint: Url,
    pub(super) model: String,
    api_key: String,
}

impl Provider {
    pub(super) fn api_key(&self) -> &str {
        &self.api_key
    }
}

pub(super) struct ExecutionSettings {
    pub(super) limits: Limits,
    pub(super) max_concurrent_requests: usize,
    pub(super) max_concurrent_executions: usize,
    pub(super) max_calls_per_execution: NonZeroU32,
}

impl ExecutionSettings {
    const DEFAULT: Self = Self {
        limits: Limits {
            timeout: Duration::from_secs(5),
            memory_limit: NonZeroUsize::new(64 * BYTES_PER_MIB).unwrap(),
            vcpu_per_execution: None,
        },
        max_concurrent_requests: 8,
        max_concurrent_executions: 4,
        max_calls_per_execution: NonZeroU32::new(32).unwrap(),
    };
}

pub(super) struct ConfigFile {
    pub(super) path: PathBuf,
    pub(super) provider: Provider,
}

pub(super) struct Config {
    pub(super) file: Option<ConfigFile>,
    pub(super) execution: ExecutionSettings,
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
    memory_limit_mb: usize,
    vcpu_per_execution: Option<f64>,
    max_concurrent_requests: usize,
    max_concurrent_executions: usize,
    max_calls_per_execution: u32,
}

impl Default for RawExecution {
    fn default() -> Self {
        let settings = ExecutionSettings::DEFAULT;
        Self {
            timeout_seconds: settings.limits.timeout.as_secs(),
            memory_limit_mb: settings.limits.memory_limit.get() / BYTES_PER_MIB,
            vcpu_per_execution: settings.limits.vcpu_per_execution,
            max_concurrent_requests: settings.max_concurrent_requests,
            max_concurrent_executions: settings.max_concurrent_executions,
            max_calls_per_execution: settings.max_calls_per_execution.get(),
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
            .checked_mul(BYTES_PER_MIB)
            .and_then(NonZeroUsize::new)
            .ok_or(ConfigError::InvalidMemoryLimit)?;
        if self
            .vcpu_per_execution
            .is_some_and(|share| !(share > 0.0 && share <= 1.0))
        {
            return Err(ConfigError::InvalidVcpuPerExecution);
        }
        if !(1..=Semaphore::MAX_PERMITS).contains(&self.max_concurrent_requests) {
            return Err(ConfigError::InvalidMaxConcurrentRequests);
        }
        if !(1..=Semaphore::MAX_PERMITS).contains(&self.max_concurrent_executions) {
            return Err(ConfigError::InvalidMaxConcurrentExecutions);
        }
        Ok(ExecutionSettings {
            limits: Limits {
                timeout: Duration::from_secs(self.timeout_seconds),
                memory_limit,
                vcpu_per_execution: self.vcpu_per_execution,
            },
            max_concurrent_requests: self.max_concurrent_requests,
            max_concurrent_executions: self.max_concurrent_executions,
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
            api_key: self.api_key,
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

pub(super) fn load(args: impl IntoIterator<Item = OsString>) -> Result<Config, ConfigError> {
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
