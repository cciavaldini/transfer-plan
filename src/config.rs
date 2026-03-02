//! Configuration handling for transfer-plan, including defaults and loading/
//! saving from JSON.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const CONFIG_FILE: &str = "defaults.json";
const CONFIG_DIR: &str = ".config/transfer-plan";
const LEGACY_CONFIG_DIR: &str = ".config/usb-transfer";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserConfig {
    #[serde(default = "default_config_version")]
    pub version: u32,
    #[serde(default = "default_source_folder")]
    pub default_source_folder: Option<PathBuf>,
    pub default_destination_folder: Option<PathBuf>,
    #[serde(default = "default_unmount_drives")]
    pub default_unmount_drives: Vec<PathBuf>,
}

fn default_config_version() -> u32 {
    1
}

fn default_source_folder() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(PathBuf::from)
        .or_else(|| Some(PathBuf::from(".")))
}

fn default_unmount_drives() -> Vec<PathBuf> {
    vec![PathBuf::from("/media/8To"), PathBuf::from("/media/16To")]
}

fn has_write_access(path: &Path) -> bool {
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let probe = path.join(format!(
        ".transfer-plan-write-check-{}-{}",
        std::process::id(),
        stamp
    ));

    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(_) => {
            let _ = fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

impl Default for UserConfig {
    fn default() -> Self {
        Self {
            version: 1,
            default_source_folder: default_source_folder(),
            default_destination_folder: None,
            default_unmount_drives: default_unmount_drives(),
        }
    }
}

impl UserConfig {
    /// Primary config file path: defaults.json in current working directory
    fn config_path() -> Result<PathBuf> {
        Ok(PathBuf::from(CONFIG_FILE))
    }

    /// Secondary config file path for the TransferPlan config directory
    fn secondary_config_path() -> Option<PathBuf> {
        std::env::var("HOME")
            .ok()
            .map(|home| PathBuf::from(home).join(CONFIG_DIR).join(CONFIG_FILE))
    }

    /// Legacy config file path kept for backward compatibility
    fn legacy_config_path() -> Option<PathBuf> {
        std::env::var("HOME").ok().map(|home| {
            PathBuf::from(home)
                .join(LEGACY_CONFIG_DIR)
                .join(CONFIG_FILE)
        })
    }

    /// Load configuration from file
    pub fn load() -> Self {
        let mut paths = vec![];
        if let Ok(primary) = Self::config_path() {
            paths.push(primary);
        }
        if let Some(secondary) = Self::secondary_config_path() {
            paths.push(secondary);
        }
        if let Some(legacy) = Self::legacy_config_path() {
            paths.push(legacy);
        }

        for path in paths {
            if path.exists() {
                match fs::read_to_string(&path) {
                    Ok(content) => match serde_json::from_str(&content) {
                        Ok(config) => return config,
                        Err(e) => eprintln!(
                            "⚠️  Warning: Config parse error in {}: {}",
                            path.display(),
                            e
                        ),
                    },
                    Err(e) => eprintln!(
                        "⚠️  Warning: Config read error in {}: {}",
                        path.display(),
                        e
                    ),
                }
            }
        }

        let config = Self::default();
        if let Err(e) = config.save() {
            eprintln!("⚠️  Warning: Failed to create {}: {}", CONFIG_FILE, e);
        }
        config
    }

    /// Save configuration to primary defaults.json file
    pub fn save(&self) -> Result<()> {
        let config_path = Self::config_path()?;

        // Create config directory if needed
        if let Some(parent) = config_path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }

        // Serialize and write config
        let json = serde_json::to_string_pretty(self)?;
        fs::write(&config_path, json)?;

        Ok(())
    }

    /// Update default source folder
    pub fn set_default_source(&mut self, path: PathBuf) -> Result<()> {
        self.default_source_folder = Some(path);
        self.save()
    }

    /// Update default destination folder
    pub fn set_default_destination(&mut self, path: PathBuf) -> Result<()> {
        self.default_destination_folder = Some(path);
        self.save()
    }

    /// Get default source folder from loaded configuration
    pub fn get_default_source(&self) -> PathBuf {
        self.default_source_folder
            .clone()
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// Get unmount drive list from loaded configuration
    pub fn get_default_unmount_drives(&self) -> Vec<PathBuf> {
        self.default_unmount_drives.clone()
    }

    /// Validate common startup config issues and return warnings.
    pub fn validate_startup(&self) -> Vec<String> {
        let mut warnings = Vec::new();

        let source = self.get_default_source();
        if !source.exists() {
            warnings.push(format!(
                "Default source folder does not exist: {}",
                source.display()
            ));
        } else if !source.is_dir() {
            warnings.push(format!(
                "Default source path is not a directory: {}",
                source.display()
            ));
        } else if fs::read_dir(&source).is_err() {
            warnings.push(format!(
                "Default source folder is not readable: {}",
                source.display()
            ));
        }

        if let Some(dest) = &self.default_destination_folder {
            if !dest.exists() {
                warnings.push(format!(
                    "Default destination folder does not exist: {}",
                    dest.display()
                ));
            } else if !dest.is_dir() {
                warnings.push(format!(
                    "Default destination path is not a directory: {}",
                    dest.display()
                ));
            } else if !has_write_access(dest) {
                warnings.push(format!(
                    "Default destination folder is not writable: {}",
                    dest.display()
                ));
            }
        }

        for mount_point in &self.default_unmount_drives {
            if !mount_point.exists() {
                warnings.push(format!(
                    "Unmount path not found (warning only): {}",
                    mount_point.display()
                ));
            }
        }

        warnings
    }
}
