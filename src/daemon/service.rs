use std::{fs, path::PathBuf, process::Command};

use color_eyre::eyre::{Context, Result, bail};

use crate::daemon::AppContext;

pub const SERVICE_NAME: &str = "ariatui-daemon.service";

pub fn install_user(app: &AppContext) -> Result<()> {
    app.paths.ensure_dirs()?;
    fs::write(&app.paths.user_service_file, build_user_unit(app))
        .wrap_err("failed to write user systemd unit")?;
    run_command(Command::new("systemctl").args(["--user", "daemon-reload"]))?;
    println!("Installed {}", app.paths.user_service_file.display());
    Ok(())
}

pub fn install_system(app: &AppContext) -> Result<()> {
    let temp_path = write_temp_unit(build_system_unit(app)?)?;
    run_command(Command::new("sudo").args([
        "install",
        "-m",
        "0644",
        temp_path.to_string_lossy().as_ref(),
        app.paths.system_service_file.to_string_lossy().as_ref(),
    ]))?;
    let _ = fs::remove_file(&temp_path);
    run_command(Command::new("sudo").args(["systemctl", "daemon-reload"]))?;
    println!("Installed {}", app.paths.system_service_file.display());
    Ok(())
}

pub fn uninstall_user(app: &AppContext) -> Result<()> {
    if app.paths.user_service_file.exists() {
        fs::remove_file(&app.paths.user_service_file).wrap_err("failed to remove user service")?;
    }
    run_command(Command::new("systemctl").args(["--user", "daemon-reload"]))?;
    println!("Removed {}", app.paths.user_service_file.display());
    Ok(())
}

pub fn uninstall_system(app: &AppContext) -> Result<()> {
    if app.paths.system_service_file.exists() {
        run_command(Command::new("sudo").args([
            "rm",
            "-f",
            app.paths.system_service_file.to_string_lossy().as_ref(),
        ]))?;
    }
    run_command(Command::new("sudo").args(["systemctl", "daemon-reload"]))?;
    println!("Removed {}", app.paths.system_service_file.display());
    Ok(())
}

pub fn install_and_enable_user(app: &AppContext) -> Result<()> {
    install_user(app)?;
    run_command(Command::new("systemctl").args(["--user", "enable", "--now", SERVICE_NAME]))
}

pub fn install_and_enable_system(app: &AppContext) -> Result<()> {
    install_system(app)?;
    run_command(Command::new("sudo").args(["systemctl", "enable", "--now", SERVICE_NAME]))
}

pub fn restart_user() -> Result<()> {
    run_command(Command::new("systemctl").args(["--user", "restart", SERVICE_NAME]))
}

pub fn restart_system() -> Result<()> {
    run_command(Command::new("sudo").args(["systemctl", "restart", SERVICE_NAME]))
}

pub fn start_user() -> Result<()> {
    run_command(Command::new("systemctl").args(["--user", "start", SERVICE_NAME]))
}

pub fn start_system() -> Result<()> {
    run_command(Command::new("sudo").args(["systemctl", "start", SERVICE_NAME]))
}

pub fn is_user_active() -> bool {
    command_succeeds(Command::new("systemctl").args([
        "--user",
        "is-active",
        "--quiet",
        SERVICE_NAME,
    ]))
}

pub fn is_system_active() -> bool {
    command_succeeds(Command::new("systemctl").args(["is-active", "--quiet", SERVICE_NAME]))
}

fn build_user_unit(app: &AppContext) -> String {
    format!(
        "[Unit]\nDescription=AriaTUI daemon\nAfter=network.target\n\n[Service]\nEnvironment=ARIATUI_BUILD_ID={}\nExecStart={} daemon\nRestart=always\nRestartSec=2\n\n[Install]\nWantedBy=default.target\n",
        app.current_build_id, app.current_executable_path
    )
}

fn build_system_unit(app: &AppContext) -> Result<String> {
    let home = std::env::var("HOME").wrap_err("HOME is not set")?;
    let user = std::env::var("USER").wrap_err("USER is not set")?;
    let uid = capture_command(Command::new("id").args(["-u"]))?;
    Ok(format!(
        "[Unit]\nDescription=AriaTUI daemon\nAfter=network.target user-runtime-dir@{uid}.service\nRequires=user-runtime-dir@{uid}.service\n\n[Service]\nUser={user}\nEnvironment=HOME={home}\nEnvironment=XDG_CONFIG_HOME={home}/.config\nEnvironment=XDG_STATE_HOME={home}/.local/state\nEnvironment=XDG_RUNTIME_DIR=/run/user/{uid}\nEnvironment=ARIATUI_BUILD_ID={}\nExecStart={} daemon\nRestart=always\nRestartSec=2\n\n[Install]\nWantedBy=multi-user.target\n",
        app.current_build_id, app.current_executable_path
    ))
}

fn write_temp_unit(contents: String) -> Result<PathBuf> {
    let path = std::env::temp_dir().join(format!("ariatui-daemon-{}.service", std::process::id()));
    fs::write(&path, contents).wrap_err_with(|| format!("failed to write {}", path.display()))?;
    Ok(path)
}

fn capture_command(command: &mut Command) -> Result<String> {
    let output = command.output()?;
    if !output.status.success() {
        bail!("command failed: {:?}", command);
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

fn run_command(command: &mut Command) -> Result<()> {
    let status = command.status()?;
    if status.success() {
        Ok(())
    } else {
        bail!("command failed with status {status}")
    }
}

fn command_succeeds(command: &mut Command) -> bool {
    command
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}
