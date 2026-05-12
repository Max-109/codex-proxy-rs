use crate::error::ProxyError;
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default)]
pub struct ProxySettings {
    pub reasoning_effort: ReasoningEffort,
    pub speed: Speed,
    pub system_messages: SystemMessages,
    pub system_prompt_file: PathBuf,
    pub host: IpAddr,
    pub port: u16,
    pub detailed_logs: bool,
    pub api_keys: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEffort {
    None,
    Minimal,
    Low,
    #[default]
    Medium,
    High,
    XHigh,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Speed {
    #[default]
    Normal,
    Fast,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SystemMessages {
    #[default]
    PassThrough,
    Ignore,
}

pub struct SettingsManager {
    settings_file: PathBuf,
}

impl ProxySettings {
    pub fn injected_system_prompt(&self) -> Result<Option<String>, ProxyError> {
        if !matches!(self.system_messages, SystemMessages::Ignore) {
            return Ok(None);
        }

        std::fs::read_to_string(&self.system_prompt_file)
            .map(Some)
            .map_err(|source| ProxyError::ReadSystemPrompt {
                path: self.system_prompt_file.clone(),
                source,
            })
    }
}

impl Default for ProxySettings {
    fn default() -> Self {
        Self {
            reasoning_effort: ReasoningEffort::Medium,
            speed: Speed::Normal,
            system_messages: SystemMessages::PassThrough,
            system_prompt_file: PathBuf::from("system.md"),
            host: IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)),
            port: 8080,
            detailed_logs: false,
            api_keys: Vec::new(),
        }
    }
}

impl ReasoningEffort {
    pub fn as_upstream_value(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
        }
    }
}

impl std::fmt::Display for ReasoningEffort {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => write!(formatter, "none"),
            Self::Minimal => write!(formatter, "minimal"),
            Self::Low => write!(formatter, "low"),
            Self::Medium => write!(formatter, "medium"),
            Self::High => write!(formatter, "high"),
            Self::XHigh => write!(formatter, "xhigh"),
        }
    }
}

impl std::fmt::Display for Speed {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Normal => write!(formatter, "normal"),
            Self::Fast => write!(formatter, "fast"),
        }
    }
}

impl std::fmt::Display for SystemMessages {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PassThrough => write!(formatter, "pass through"),
            Self::Ignore => write!(formatter, "ignore"),
        }
    }
}

impl SettingsManager {
    pub fn new(settings_file: PathBuf) -> Self {
        Self { settings_file }
    }

    pub fn default_settings_file() -> PathBuf {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".codex-proxy")
            .join("settings.json")
    }

    pub fn load(&self) -> Result<ProxySettings, ProxyError> {
        match std::fs::read_to_string(&self.settings_file) {
            Ok(settings_json) => {
                serde_json::from_str(&settings_json).map_err(ProxyError::ParseSettings)
            }
            Err(read_error) if read_error.kind() == std::io::ErrorKind::NotFound => {
                Ok(ProxySettings::default())
            }
            Err(read_error) => Err(ProxyError::ReadSettings(read_error)),
        }
    }

    pub fn save(&self, settings: &ProxySettings) -> Result<(), ProxyError> {
        if let Some(settings_dir) = self.settings_file.parent() {
            std::fs::create_dir_all(settings_dir).map_err(ProxyError::WriteSettings)?;
        }

        let mut open_options = OpenOptions::new();
        open_options.create(true).truncate(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            open_options.mode(0o600);
        }

        let settings_json =
            serde_json::to_string_pretty(settings).map_err(ProxyError::ParseSettings)?;
        let mut settings_file = open_options
            .open(&self.settings_file)
            .map_err(ProxyError::WriteSettings)?;
        settings_file
            .write_all(settings_json.as_bytes())
            .map_err(ProxyError::WriteSettings)?;
        settings_file.flush().map_err(ProxyError::WriteSettings)
    }

    pub fn settings_file(&self) -> &PathBuf {
        &self.settings_file
    }
}
