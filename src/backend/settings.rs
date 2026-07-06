use std::collections::HashMap;
use std::path::PathBuf;

const DEFAULT_SHELL: &str = "/bin/bash";

#[derive(Debug, Clone)]
pub struct BackendSettings {
    pub shell: String,
    pub args: Vec<String>,
    pub working_directory: Option<PathBuf>,
    /// Extra environment variables for the spawned process, merged into the
    /// inherited environment (custom values win). Lets the host inject env
    /// (e.g. KUBECONFIG) without a Unix-only `/usr/bin/env` wrapper.
    pub env: HashMap<String, String>,
}

impl Default for BackendSettings {
    fn default() -> Self {
        Self {
            shell: DEFAULT_SHELL.to_string(),
            args: vec![],
            working_directory: None,
            env: HashMap::new(),
        }
    }
}
