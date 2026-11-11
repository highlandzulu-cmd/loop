//! A Session's caller environment layered over its confined filesystem and shell.
use super::super::types::*;
use async_trait::async_trait;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::Arc,
};

/// Keeps a caller environment separate from the owner process.
pub struct SessionExecutionEnv {
    inner: Arc<dyn ExecutionEnv>,
    env: BTreeMap<String, String>,
}
impl SessionExecutionEnv {
    /// Preserve the filesystem and sandbox while replacing inherited variables.
    pub fn wrap(
        inner: Arc<dyn ExecutionEnv>,
        env: &BTreeMap<String, String>,
    ) -> Arc<dyn ExecutionEnv> {
        Arc::new(Self {
            inner,
            env: env
                .iter()
                .filter(|(key, _)| {
                    key.as_str() != "LOOP_RUNTIME_TOKEN"
                        && !loop_ai::providers::PROVIDER_PRESETS
                            .iter()
                            .any(|p| p.api_key_envs.contains(&key.as_str()))
                })
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        })
    }
}
#[async_trait]
impl Shell for SessionExecutionEnv {
    async fn exec(
        &self,
        command: &str,
        mut options: ShellExecOptions,
    ) -> Result<ShellOutput, ExecutionError> {
        let mut env: std::collections::HashMap<_, _> = if options.inherit_env {
            self.env.clone().into_iter().collect()
        } else {
            Default::default()
        };
        if let Some(overrides) = options.env.take() {
            env.extend(overrides);
        }
        options.env = Some(env);
        options.inherit_env = false;
        self.inner.exec(command, options).await
    }
}
#[async_trait]
impl FileSystem for SessionExecutionEnv {
    fn cwd(&self) -> &Path {
        self.inner.cwd()
    }
    fn absolute_path(&self, path: &Path) -> Result<PathBuf, FileError> {
        self.inner.absolute_path(path)
    }
    fn join_path(&self, base: &Path, child: &Path) -> PathBuf {
        self.inner.join_path(base, child)
    }
    async fn read_text_file(&self, path: &Path) -> Result<String, FileError> {
        self.inner.read_text_file(path).await
    }
    async fn read_binary_file(&self, path: &Path) -> Result<Vec<u8>, FileError> {
        self.inner.read_binary_file(path).await
    }
    async fn write_file(&self, path: &Path, data: &[u8]) -> Result<(), FileError> {
        self.inner.write_file(path, data).await
    }
    async fn append_file(&self, path: &Path, data: &[u8]) -> Result<(), FileError> {
        self.inner.append_file(path, data).await
    }
    async fn file_info(&self, path: &Path) -> Result<FileInfo, FileError> {
        self.inner.file_info(path).await
    }
    async fn list_dir(&self, path: &Path) -> Result<Vec<FileInfo>, FileError> {
        self.inner.list_dir(path).await
    }
    async fn exists(&self, path: &Path) -> Result<bool, FileError> {
        self.inner.exists(path).await
    }
    async fn create_dir(&self, path: &Path) -> Result<(), FileError> {
        self.inner.create_dir(path).await
    }
    async fn remove(&self, path: &Path) -> Result<(), FileError> {
        self.inner.remove(path).await
    }
    async fn canonical_path(&self, path: &Path) -> Result<PathBuf, FileError> {
        self.inner.canonical_path(path).await
    }
    async fn create_temp_dir(&self, prefix: &str) -> Result<PathBuf, FileError> {
        self.inner.create_temp_dir(prefix).await
    }
    async fn file_identity(
        &self,
        path: &Path,
    ) -> Result<Option<crate::harness::types::FileIdentity>, FileError> {
        self.inner.file_identity(path).await
    }
}
