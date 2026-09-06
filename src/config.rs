#![allow(clippy::missing_docs_in_private_items)] // 9 left to document
//! Configuration file and CLI arguments.
//!
//! The config file is optional. Unknown keys are rejected rather than ignored,
//! so a typo is a loud error instead of a silently missing pin.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

pub const FILE_NAME: &str = ".composemux.yaml";

/// Seconds to count down before auto-exiting once every service has finished.
/// Matches nx's `AutoExit::DEFAULT_COUNTDOWN_SECONDS`.
pub const DEFAULT_AUTO_EXIT_SECONDS: u64 = 3;
pub const DEFAULT_TAIL: usize = 200;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Compose project name. Usually supplied as `--project` instead.
    #[serde(default)]
    pub project: Option<String>,
    /// Services to show. Empty means all of them.
    #[serde(default)]
    pub include: Vec<String>,
    /// Services to hide. Applied after `include`.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Services pinned to output panes 1 and 2 at startup.
    #[serde(default)]
    pub pinned: Vec<String>,
    /// Lines of history to load per service before following.
    #[serde(default = "default_tail")]
    pub tail: usize,
    /// Rows of output retained per service.
    ///
    /// Costs roughly 7 MB per service at the default, scaling linearly, so
    /// raising it on a large project is a real memory trade. It also sets how
    /// long a scrolled-up pane can hold its position: the view only moves once
    /// the lines in it fall out of the buffer.
    #[serde(default = "default_scrollback")]
    pub scrollback: usize,
    #[serde(default)]
    pub auto_exit: AutoExit,
}

fn default_tail() -> usize {
    DEFAULT_TAIL
}

fn default_scrollback() -> usize {
    crate::model::DEFAULT_SCROLLBACK
}

impl Default for Config {
    fn default() -> Self {
        Self {
            project: None,
            include: Vec::new(),
            exclude: Vec::new(),
            pinned: Vec::new(),
            tail: DEFAULT_TAIL,
            scrollback: crate::model::DEFAULT_SCROLLBACK,
            auto_exit: AutoExit::default(),
        }
    }
}

/// `auto_exit` accepts either a bool or a number of seconds, as nx's does.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(untagged)]
pub enum AutoExit {
    Enabled(bool),
    Seconds(u64),
}

impl Default for AutoExit {
    fn default() -> Self {
        AutoExit::Seconds(DEFAULT_AUTO_EXIT_SECONDS)
    }
}

impl AutoExit {
    /// Countdown length, or `None` when auto-exit is disabled.
    pub fn seconds(self) -> Option<u64> {
        match self {
            AutoExit::Enabled(false) => None,
            AutoExit::Enabled(true) => Some(DEFAULT_AUTO_EXIT_SECONDS),
            AutoExit::Seconds(s) => Some(s),
        }
    }
}

impl Config {
    /// Loads `path` if given, else the nearest `.composemux.yaml` walking up
    /// from the working directory, else the user config dir. A missing file is
    /// not an error.
    pub fn load(explicit: Option<&Path>) -> Result<Self> {
        let path = match explicit {
            Some(p) => Some(p.to_path_buf()),
            None => discover(),
        };
        let Some(path) = path else {
            return Ok(Self::default());
        };
        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("could not read {}", path.display()))?;
        serde_yaml_ng::from_str(&raw).with_context(|| format!("could not parse {}", path.display()))
    }

    /// Whether a service should be shown, per `include`/`exclude`.
    pub fn is_visible(&self, service: &str) -> bool {
        if self.exclude.iter().any(|e| e == service) {
            return false;
        }
        self.include.is_empty() || self.include.iter().any(|i| i == service)
    }
}

fn discover() -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    for dir in cwd.ancestors() {
        let candidate = dir.join(FILE_NAME);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    let global = dirs_config_dir()?.join("composemux").join("config.yaml");
    global.is_file().then_some(global)
}

/// `$XDG_CONFIG_HOME`, else `~/.config`, else `%APPDATA%` / `%USERPROFILE%` on
/// Windows, which sets neither `XDG_CONFIG_HOME` nor `HOME`.
fn dirs_config_dir() -> Option<PathBuf> {
    config_dir_from(|key| std::env::var(key).ok())
}

/// Split out from the environment so the precedence rules can be tested without
/// mutating process-global state.
fn config_dir_from(env: impl Fn(&str) -> Option<String>) -> Option<PathBuf> {
    let non_empty = |key: &str| env(key).filter(|v| !v.is_empty());
    if let Some(xdg) = non_empty("XDG_CONFIG_HOME") {
        return Some(PathBuf::from(xdg));
    }
    if let Some(home) = non_empty("HOME") {
        return Some(PathBuf::from(home).join(".config"));
    }
    if let Some(appdata) = non_empty("APPDATA") {
        return Some(PathBuf::from(appdata));
    }
    non_empty("USERPROFILE").map(|p| PathBuf::from(p).join(".config"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_apply_to_an_empty_document() {
        let c: Config = serde_yaml_ng::from_str("{}").unwrap();
        assert_eq!(c.tail, DEFAULT_TAIL);
        assert_eq!(c.scrollback, crate::model::DEFAULT_SCROLLBACK);
        assert_eq!(c.auto_exit.seconds(), Some(DEFAULT_AUTO_EXIT_SECONDS));
        assert!(c.pinned.is_empty());
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let err = serde_yaml_ng::from_str::<Config>("pined: [api]").unwrap_err();
        assert!(err.to_string().contains("pined"), "got: {err}");
    }

    #[test]
    fn auto_exit_accepts_bool_or_seconds() {
        let off: Config = serde_yaml_ng::from_str("auto_exit: false").unwrap();
        assert_eq!(off.auto_exit.seconds(), None);
        let ten: Config = serde_yaml_ng::from_str("auto_exit: 10").unwrap();
        assert_eq!(ten.auto_exit.seconds(), Some(10));
    }

    #[test]
    fn xdg_config_home_wins_when_set() {
        let dir = config_dir_from(|k| match k {
            "XDG_CONFIG_HOME" => Some("/xdg".into()),
            "HOME" => Some("/home/u".into()),
            _ => None,
        });
        assert_eq!(dir, Some(PathBuf::from("/xdg")));
    }

    #[test]
    fn home_is_used_when_xdg_is_unset_or_empty() {
        let dir = config_dir_from(|k| match k {
            "XDG_CONFIG_HOME" => Some(String::new()),
            "HOME" => Some("/home/u".into()),
            _ => None,
        });
        assert_eq!(dir, Some(PathBuf::from("/home/u/.config")));
    }

    #[test]
    fn windows_falls_back_to_appdata_then_userprofile() {
        let appdata = config_dir_from(|k| match k {
            "APPDATA" => Some("C:\\Users\\u\\AppData\\Roaming".into()),
            "USERPROFILE" => Some("C:\\Users\\u".into()),
            _ => None,
        });
        assert_eq!(
            appdata,
            Some(PathBuf::from("C:\\Users\\u\\AppData\\Roaming"))
        );

        let profile = config_dir_from(|k| match k {
            "USERPROFILE" => Some("C:\\Users\\u".into()),
            _ => None,
        });
        assert_eq!(profile, Some(PathBuf::from("C:\\Users\\u").join(".config")));
    }

    #[test]
    fn no_environment_yields_no_config_dir() {
        assert_eq!(config_dir_from(|_| None), None);
    }

    #[test]
    fn include_and_exclude_filter_services() {
        let c: Config = serde_yaml_ng::from_str("include: [api, db]\nexclude: [db]").unwrap();
        assert!(c.is_visible("api"));
        assert!(!c.is_visible("db"));
        assert!(!c.is_visible("worker"));
    }
}
