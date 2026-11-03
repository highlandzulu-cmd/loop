//! Host-owned choices for one workspace (`workspace/mcp`, `workspace/skills`,
//! `workspace/web_search`), kept in the agent directory keyed by canonical
//! cwd. Never in a project's own files: a project cannot loosen them, and
//! they never change another workspace or the global defaults.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::Settings;

/// One workspace's overrides. An absent field inherits the merged settings.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceOverrides {
    /// Replaces `disabledSkills`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_skills: Option<Vec<String>>,
    /// Replaces `disabledMcpServers`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_mcp_servers: Option<Vec<String>>,
    /// Replaces `webSearch`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web_search: Option<loop_agent::harness::WebSearchConfig>,
}

impl WorkspaceOverrides {
    /// Apply over global and project settings.
    pub fn apply(&self, settings: &mut Settings) {
        if let Some(list) = &self.disabled_skills {
            settings.disabled_skills = list.clone();
        }
        if let Some(list) = &self.disabled_mcp_servers {
            settings.disabled_mcp_servers = list.clone();
        }
        if let Some(web) = &self.web_search {
            settings.web_search = Some(web.clone());
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct File {
    #[serde(default)]
    workspaces: BTreeMap<String, WorkspaceOverrides>,
}

/// `workspace-settings.json` in the agent directory.
pub struct WorkspaceSettingsStore {
    path: PathBuf,
    file: File,
}

impl WorkspaceSettingsStore {
    /// Read the store; a missing file is empty.
    pub fn load(agent_dir: &Path) -> anyhow::Result<Self> {
        let path = agent_dir.join("workspace-settings.json");
        let file = match std::fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str(&raw)?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => File::default(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self { path, file })
    }

    fn key(cwd: &Path) -> String {
        cwd.canonicalize()
            .unwrap_or_else(|_| cwd.to_path_buf())
            .to_string_lossy()
            .into_owned()
    }

    /// `cwd`'s overrides.
    pub fn get(&self, cwd: &Path) -> WorkspaceOverrides {
        self.file
            .workspaces
            .get(&Self::key(cwd))
            .cloned()
            .unwrap_or_default()
    }

    /// Replace `cwd`'s overrides and save (written whole, then renamed).
    pub fn set(&mut self, cwd: &Path, overrides: WorkspaceOverrides) -> anyhow::Result<()> {
        self.file.workspaces.insert(Self::key(cwd), overrides);
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let temp = self.path.with_extension("json.tmp");
        std::fs::write(&temp, serde_json::to_string_pretty(&self.file)?)?;
        std::fs::rename(&temp, &self.path)?;
        Ok(())
    }
}
