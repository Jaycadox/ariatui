use std::{
    fs,
    io::{self, Write},
    path::Path,
    time::Duration,
};

use color_eyre::eyre::{Result, eyre};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    time::timeout,
};

use crate::{
    binary_info::read_unit_metadata,
    daemon::{ApiEnvelope, ApiRequest, ApiResponse, AppContext, Snapshot, marker, service},
    paths::AppPaths,
};

#[derive(Debug, Clone)]
pub enum BootstrapAction {
    StartUi { initial_snapshot: Box<Option<Snapshot>> },
    Exit,
}

#[derive(Debug, Clone)]
struct ServiceInstallStatus {
    user_installed: bool,
    system_installed: bool,
    user_active: bool,
    system_active: bool,
    user_exec_path: Option<String>,
    system_exec_path: Option<String>,
    user_build_id: Option<String>,
    system_build_id: Option<String>,
}

pub async fn run_default_flow(app: &AppContext) -> Result<BootstrapAction> {
    if let Some(runtime_marker) = read_live_daemon_marker(&app.paths)? {
        if runtime_marker.build_id == app.current_build_id {
            let initial_snapshot = read_snapshot_cache(&app.paths);
            return Ok(BootstrapAction::StartUi {
                initial_snapshot: Box::new(initial_snapshot),
            });
        } else {
            eprintln!(
                "AriaTUI fast path rejected .daemon marker: running pid {} build {} but current build is {}",
                runtime_marker.pid, runtime_marker.build_id, app.current_build_id
            );
        }
    }

    let service_status = detect_service_install_status(&app.paths)?;
    let running_snapshot = fetch_daemon_snapshot_with_timeout(&app.paths, 400, 800)
        .await
        .ok();

    if let Some(snapshot) = running_snapshot.as_ref() {
        let snapshot = handle_outdated_daemon(app, snapshot, &service_status).await?;
        return Ok(BootstrapAction::StartUi {
            initial_snapshot: Box::new(Some(snapshot)),
        });
    }

    if !is_arch_linux()? || !has_systemd() {
        return Ok(BootstrapAction::StartUi {
            initial_snapshot: Box::new(None),
        });
    }

    println!("AriaTUI daemon is not running.");
    println!(
        "User service installed: {} | System service installed: {}",
        yes_no(service_status.user_installed),
        yes_no(service_status.system_installed)
    );
    println!(
        "User service active: {} | System service active: {}",
        yes_no(service_status.user_active),
        yes_no(service_status.system_active)
    );
    print_service_binary_status("user", &service_status.user_build_id, &app.current_build_id);
    print_service_binary_status(
        "system",
        &service_status.system_build_id,
        &app.current_build_id,
    );
    println!("Prepare the daemon before opening the UI?");
    println!(
        "  [u] {}",
        service_action_label(
            service_status.user_installed,
            service_status.user_active,
            service_status.user_build_id.as_deref() == Some(app.current_build_id.as_str()),
            "user"
        )
    );
    println!(
        "  [s] {}",
        service_action_label(
            service_status.system_installed,
            service_status.system_active,
            service_status.system_build_id.as_deref() == Some(app.current_build_id.as_str()),
            "system"
        )
    );
    println!("  [c] continue to UI without installing");
    println!("  [q] quit");

    match prompt_choice("Choice", &['u', 's', 'c', 'q'])? {
        'u' => {
            ensure_user_service_ready(app, &service_status)?;
            let snapshot =
                wait_for_daemon_build_id(&app.paths, app.current_build_id.as_str()).await?;
            Ok(BootstrapAction::StartUi {
                initial_snapshot: Box::new(Some(snapshot)),
            })
        }
        's' => {
            ensure_system_service_ready(app, &service_status)?;
            let snapshot =
                wait_for_daemon_build_id(&app.paths, app.current_build_id.as_str()).await?;
            Ok(BootstrapAction::StartUi {
                initial_snapshot: Box::new(Some(snapshot)),
            })
        }
        'c' => Ok(BootstrapAction::StartUi {
            initial_snapshot: Box::new(None),
        }),
        'q' => Ok(BootstrapAction::Exit),
        _ => unreachable!(),
    }
}

async fn handle_outdated_daemon(
    app: &AppContext,
    snapshot: &Snapshot,
    service_status: &ServiceInstallStatus,
) -> Result<Snapshot> {
    println!("AriaTUI daemon is running an older or different binary.");
    println!("  daemon: {}", snapshot.daemon_status.executable_path);
    println!("  current: {}", app.current_executable_path);
    println!("  daemon build id: {}", snapshot.daemon_status.build_id);
    println!("  current build id: {}", app.current_build_id);

    if service_status.user_installed || service_status.system_installed {
        println!("Update the installed service and restart the daemon?");
        if service_status.user_installed {
            println!(
                "  user service target: {}",
                describe_service_target(
                    &service_status.user_exec_path,
                    &service_status.user_build_id
                )
            );
        }
        if service_status.system_installed {
            println!(
                "  system service target: {}",
                describe_service_target(
                    &service_status.system_exec_path,
                    &service_status.system_build_id
                )
            );
        }
        println!("  [u] update/restart user service");
        println!("  [s] update/restart system service");
        println!("  [n] keep current daemon");

        let choices = if service_status.user_installed && service_status.system_installed {
            vec!['u', 's', 'n']
        } else if service_status.user_installed {
            vec!['u', 'n']
        } else {
            vec!['s', 'n']
        };

        match prompt_choice("Choice", &choices)? {
            'u' => {
                restart_user_service_for_update(app, service_status)?;
                wait_for_daemon_build_id(&app.paths, app.current_build_id.as_str()).await
            }
            's' => {
                restart_system_service_for_update(app, service_status)?;
                wait_for_daemon_build_id(&app.paths, app.current_build_id.as_str()).await
            }
            'n' => Ok(snapshot.clone()),
            _ => unreachable!(),
        }
    } else {
        println!("No managed systemd service was found. Continuing with the running daemon.");
        Ok(snapshot.clone())
    }
}

fn detect_service_install_status(paths: &AppPaths) -> Result<ServiceInstallStatus> {
    let user_metadata = read_unit_metadata(&paths.user_service_file)?;
    let system_metadata = read_unit_metadata(&paths.system_service_file)?;
    Ok(ServiceInstallStatus {
        user_installed: paths.user_service_file.exists(),
        system_installed: paths.system_service_file.exists(),
        user_active: service::is_user_active(),
        system_active: service::is_system_active(),
        user_exec_path: user_metadata
            .exec_path
            .as_ref()
            .map(|path| path.display().to_string()),
        system_exec_path: system_metadata
            .exec_path
            .as_ref()
            .map(|path| path.display().to_string()),
        user_build_id: user_metadata.build_id,
        system_build_id: system_metadata.build_id,
    })
}

fn read_live_daemon_marker(paths: &AppPaths) -> Result<Option<marker::DaemonMarkerInfo>> {
    if !paths.daemon_marker_file.exists() {
        return Ok(None);
    }

    let info = match marker::read(&paths.daemon_marker_file) {
        Ok(info) => info,
        Err(error) => {
            eprintln!(
                "AriaTUI fast path rejected .daemon marker {}: {error}",
                paths.daemon_marker_file.display()
            );
            return Ok(None);
        }
    };

    if process_is_alive(info.pid) {
        Ok(Some(info))
    } else {
        eprintln!(
            "AriaTUI fast path rejected .daemon marker: pid {} is not alive, removing stale marker {}",
            info.pid,
            paths.daemon_marker_file.display()
        );
        let _ = fs::remove_file(&paths.daemon_marker_file);
        Ok(None)
    }
}

fn read_snapshot_cache(paths: &AppPaths) -> Option<Snapshot> {
    if !paths.snapshot_cache_file.exists() {
        return None;
    }
    match fs::read(&paths.snapshot_cache_file) {
        Ok(contents) => match serde_json::from_slice::<Snapshot>(&contents) {
            Ok(snapshot) => Some(snapshot),
            Err(error) => {
                eprintln!(
                    "AriaTUI fast path ignored snapshot cache {}: {error}",
                    paths.snapshot_cache_file.display()
                );
                None
            }
        },
        Err(error) => {
            eprintln!(
                "AriaTUI fast path ignored snapshot cache {}: {error}",
                paths.snapshot_cache_file.display()
            );
            None
        }
    }
}

async fn fetch_daemon_snapshot_with_timeout(
    paths: &AppPaths,
    connect_timeout_ms: u64,
    read_timeout_ms: u64,
) -> Result<Snapshot> {
    let mut stream = timeout(
        Duration::from_millis(connect_timeout_ms),
        UnixStream::connect(&paths.socket_path),
    )
    .await
    .map_err(|_| eyre!("timed out connecting to daemon"))??;
    let payload = serde_json::to_vec(&ApiEnvelope {
        id: "bootstrap-get-snapshot".into(),
        request: ApiRequest::GetSnapshot,
    })?;
    stream.write_all(&payload).await?;
    stream.write_all(b"\n").await?;
    stream.flush().await?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    timeout(
        Duration::from_millis(read_timeout_ms),
        reader.read_line(&mut line),
    )
    .await
    .map_err(|_| eyre!("timed out waiting for daemon snapshot"))??;
    let response: ApiResponse = serde_json::from_str(&line)?;
    if !response.ok {
        return Err(eyre!(
            "{}",
            response
                .error
                .map(|error| error.message)
                .unwrap_or_else(|| "daemon snapshot request failed".into())
        ));
    }
    response
        .result
        .ok_or_else(|| eyre!("daemon returned no snapshot"))
}

async fn wait_for_daemon_build_id(paths: &AppPaths, expected_build_id: &str) -> Result<Snapshot> {
    for _ in 0..24 {
        if let Ok(snapshot) = fetch_daemon_snapshot_with_timeout(paths, 300, 800).await
            && snapshot.daemon_status.build_id == expected_build_id
        {
            return Ok(snapshot);
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    Err(eyre!(
        "daemon did not come back with the expected build id after updating the service"
    ))
}

fn prompt_choice(label: &str, allowed: &[char]) -> Result<char> {
    loop {
        print!("{label}: ");
        io::stdout().flush()?;
        let mut input = String::new();
        io::stdin().read_line(&mut input)?;
        let choice = input
            .trim()
            .chars()
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if allowed.contains(&choice) {
            return Ok(choice);
        }
        println!(
            "Please enter one of: {}",
            allowed.iter().collect::<String>()
        );
    }
}

fn is_arch_linux() -> Result<bool> {
    let os_release = fs::read_to_string("/etc/os-release").unwrap_or_default();
    Ok(os_release.lines().any(|line| line == "ID=arch"))
}

fn has_systemd() -> bool {
    Path::new("/run/systemd/system").exists() && which::which("systemctl").is_ok()
}

fn process_is_alive(pid: u32) -> bool {
    let pid = pid as i32;
    if pid <= 0 {
        return false;
    }
    // SAFETY: libc::kill with signal 0 only probes process existence.
    let result = unsafe { libc::kill(pid, 0) };
    if result == 0 {
        true
    } else {
        matches!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::EPERM)
        )
    }
}

fn yes_no(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

fn describe_service_target(path: &Option<String>, build_id: &Option<String>) -> String {
    match (path, build_id) {
        (Some(path), Some(build_id)) => format!("{path} (build id {build_id})"),
        (Some(path), None) => format!("{path} (missing build id metadata)"),
        (None, _) => "unknown".into(),
    }
}

fn print_service_binary_status(
    label: &str,
    service_build_id: &Option<String>,
    current_build_id: &str,
) {
    match service_build_id {
        Some(build_id) if build_id != current_build_id => {
            println!("  existing {label} service targets a different build id");
        }
        Some(_) => {
            println!("  existing {label} service already targets this binary");
        }
        None => {}
    }
}

fn ensure_user_service_ready(app: &AppContext, status: &ServiceInstallStatus) -> Result<()> {
    if !status.user_installed {
        return service::install_and_enable_user(app);
    }
    if status.user_build_id.as_deref() != Some(app.current_build_id.as_str()) {
        service::install_user(app)?;
        if status.user_active {
            service::restart_user()
        } else {
            service::start_user()
        }
    } else if status.user_active {
        Ok(())
    } else {
        service::start_user()
    }
}

fn ensure_system_service_ready(app: &AppContext, status: &ServiceInstallStatus) -> Result<()> {
    if !status.system_installed {
        return service::install_and_enable_system(app);
    }
    if status.system_build_id.as_deref() != Some(app.current_build_id.as_str()) {
        service::install_system(app)?;
        if status.system_active {
            service::restart_system()
        } else {
            service::start_system()
        }
    } else if status.system_active {
        Ok(())
    } else {
        service::start_system()
    }
}

fn restart_user_service_for_update(app: &AppContext, status: &ServiceInstallStatus) -> Result<()> {
    if !status.user_installed {
        return service::install_and_enable_user(app);
    }
    if status.user_build_id.as_deref() != Some(app.current_build_id.as_str()) {
        service::install_user(app)?;
    }
    if status.user_active {
        service::restart_user()
    } else {
        service::start_user()
    }
}

fn restart_system_service_for_update(
    app: &AppContext,
    status: &ServiceInstallStatus,
) -> Result<()> {
    if !status.system_installed {
        return service::install_and_enable_system(app);
    }
    if status.system_build_id.as_deref() != Some(app.current_build_id.as_str()) {
        service::install_system(app)?;
    }
    if status.system_active {
        service::restart_system()
    } else {
        service::start_system()
    }
}

fn service_action_label(installed: bool, active: bool, current_hash: bool, scope: &str) -> String {
    match (installed, active, current_hash) {
        (false, _, _) => format!("install and enable {scope} service"),
        (true, false, true) => format!("start installed {scope} service"),
        (true, true, true) => format!("use installed running {scope} service"),
        (true, _, false) => format!("update and restart installed {scope} service"),
    }
}
