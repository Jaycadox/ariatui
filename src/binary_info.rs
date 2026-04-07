use std::{
    fs,
    path::{Path, PathBuf},
};

use color_eyre::eyre::{Context, Result};

#[derive(Debug, Clone, Default)]
pub struct UnitMetadata {
    pub exec_path: Option<PathBuf>,
    pub build_id: Option<String>,
}

pub fn current_executable_path() -> Result<PathBuf> {
    std::env::current_exe().wrap_err("failed to resolve current executable path")
}

pub fn current_build_id() -> String {
    env!("ARIATUI_BUILD_ID").to_string()
}

pub fn read_unit_metadata(unit_path: &Path) -> Result<UnitMetadata> {
    if !unit_path.exists() {
        return Ok(UnitMetadata::default());
    }
    let content = fs::read_to_string(unit_path)
        .wrap_err_with(|| format!("failed to read {}", unit_path.display()))?;
    let mut metadata = UnitMetadata::default();
    for line in content.lines() {
        if let Some(command) = line.strip_prefix("ExecStart=") {
            metadata.exec_path = command.split_whitespace().next().map(PathBuf::from);
        } else if let Some(value) = line.strip_prefix("Environment=ARIATUI_BUILD_ID=") {
            metadata.build_id = Some(value.trim().to_string());
        }
    }
    Ok(metadata)
}
