use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use serde::Deserialize;

use crate::model::Category;

static CONFIG: LazyLock<Config> = LazyLock::new(Config::load);

/// Loaded once from `~/.config/wyd/config.toml`. Missing/invalid file → defaults.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    #[serde(default)]
    pub leftovers: LeftoverConfig,
    #[serde(default)]
    pub persistent: PersistentConfig,
    #[serde(default)]
    pub projects: ProjectsConfig,
    #[serde(default)]
    pub signature: Vec<SignatureConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LeftoverConfig {
    pub server_age_hours: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct PersistentConfig {
    pub commands: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ProjectsConfig {
    pub roots: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SignatureConfig {
    pub category: String,
    pub names: Vec<String>,
    pub contains: Vec<String>,
    pub display: String,
}

impl Default for LeftoverConfig {
    fn default() -> Self {
        Self {
            server_age_hours: 8,
        }
    }
}

impl Config {
    pub fn global() -> &'static Config {
        &CONFIG
    }
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            return Self::default();
        };
        Self::load_from(&path)
    }

    pub fn load_from(path: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        toml::from_str(&text).unwrap_or_default()
    }

    pub fn project_roots(&self) -> Vec<PathBuf> {
        self.projects.roots.iter().map(|r| expand_home(r)).collect()
    }
}

impl SignatureConfig {
    pub fn category(&self) -> Option<Category> {
        match self.category.to_ascii_lowercase().as_str() {
            "agent" => Some(Category::Agent),
            "mcp" => Some(Category::Mcp),
            "devserver" | "dev-server" => Some(Category::DevServer),
            "database" => Some(Category::Database),
            "languageserver" | "lsp" => Some(Category::LanguageServer),
            _ => None,
        }
    }
}

fn config_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".config/wyd/config.toml"))
}

pub fn expand_home(raw: &str) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    PathBuf::from(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_thresholds_and_signatures() {
        let dir = std::env::temp_dir().join(format!("wyd-cfg-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            r#"
[leftovers]
server_age_hours = 12

[persistent]
commands = ["my-daemon"]

[projects]
roots = ["~/Work"]

[[signature]]
category = "agent"
names = ["myagent"]
contains = ["my-company-agent"]
display = "myagent"
"#,
        )
        .unwrap();
        let cfg = Config::load_from(&path);
        assert_eq!(cfg.leftovers.server_age_hours, 12);
        assert_eq!(cfg.persistent.commands, ["my-daemon"]);
        assert_eq!(cfg.signature[0].names, ["myagent"]);
        assert_eq!(cfg.signature[0].category(), Some(Category::Agent));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_file_is_defaults() {
        let cfg = Config::load_from(Path::new("/no/such/wyd.toml"));
        assert_eq!(cfg.leftovers.server_age_hours, 8);
        assert!(cfg.signature.is_empty());
    }
}
