use std::{
    fs,
    path::{Path, PathBuf},
};

use color_eyre::eyre::{Context, Result, eyre};
use directories::ProjectDirs;

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub config_dir: PathBuf,
    pub state_dir: PathBuf,
    pub runtime_dir: PathBuf,
    pub config_file: PathBuf,
    pub state_file: PathBuf,
    pub socket_path: PathBuf,
    pub daemon_marker_file: PathBuf,
    pub snapshot_cache_file: PathBuf,
    pub aria2_session_file: PathBuf,
    pub user_service_dir: PathBuf,
    pub user_service_file: PathBuf,
    pub system_service_file: PathBuf,
}

impl AppPaths {
    pub fn discover() -> Result<Self> {
        let dirs = ProjectDirs::from("dev", "jayphen", "ariatui")
            .ok_or_else(|| eyre!("failed to determine XDG directories"))?;
        let config_dir = dirs.config_dir().to_path_buf();
        let state_dir = dirs
            .state_dir()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| dirs.data_local_dir().join("state"));
        let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| state_dir.clone())
            .join("ariatui");
        let config_file = config_dir.join("config.toml");
        let state_file = state_dir.join("state.toml");
        let socket_path = runtime_dir.join("daemon.sock");
        let daemon_marker_file = runtime_dir.join(".daemon");
        let snapshot_cache_file = runtime_dir.join(".snapshot");
        let aria2_session_file = state_dir.join("aria2.session");
        let user_service_dir = config_dir
            .parent()
            .map(|base| base.join("systemd/user"))
            .unwrap_or_else(|| {
                dirs.config_local_dir()
                    .parent()
                    .map(|base| base.join("systemd/user"))
                    .unwrap_or_else(|| config_dir.join("systemd/user"))
            });
        let user_service_file = user_service_dir.join("ariatui-daemon.service");
        let system_service_file = PathBuf::from("/etc/systemd/system/ariatui-daemon.service");

        Ok(Self {
            config_dir,
            state_dir,
            runtime_dir,
            config_file,
            state_file,
            socket_path,
            daemon_marker_file,
            snapshot_cache_file,
            aria2_session_file,
            user_service_dir,
            user_service_file,
            system_service_file,
        })
    }

    pub fn ensure_dirs(&self) -> Result<()> {
        for dir in [
            &self.config_dir,
            &self.state_dir,
            &self.runtime_dir,
            &self.user_service_dir,
        ] {
            fs::create_dir_all(dir)
                .wrap_err_with(|| format!("failed to create {}", dir.display()))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_expected_files() {
        let paths = AppPaths::discover().expect("paths");
        assert!(paths.config_file.ends_with("config.toml"));
        assert!(paths.state_file.ends_with("state.toml"));
        assert!(paths.socket_path.ends_with("daemon.sock"));
        assert!(paths.daemon_marker_file.ends_with(".daemon"));
        assert!(paths.snapshot_cache_file.ends_with(".snapshot"));
        assert!(
            paths
                .user_service_file
                .ends_with("systemd/user/ariatui-daemon.service")
        );
    }
}
