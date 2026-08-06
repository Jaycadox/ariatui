use std::{
    collections::HashSet,
    fs,
    os::unix::fs::MetadataExt,
    path::{Component, Path, PathBuf},
};

use color_eyre::eyre::{Result, bail, eyre};

use crate::{
    daemon::{ApiRequest, SharedDaemonState},
    download_uri::magnet_display_name,
    routing::{expand_home, match_rule},
};

#[derive(Debug, Clone, Copy)]
pub struct PeerIdentity {
    pub uid: u32,
    pub gid: u32,
    pub pid: Option<u32>,
}

pub async fn validate_request(
    state: &SharedDaemonState,
    request: &mut ApiRequest,
    peer: PeerIdentity,
) -> Result<()> {
    // A same-UID daemon already operates with exactly the caller's filesystem authority.
    if unsafe { libc::geteuid() } == peer.uid {
        return Ok(());
    }
    match request {
        ApiRequest::AddDownload {
            url,
            filename,
            directory,
            ..
        } => {
            let destination = if let Some(directory) = directory.as_deref() {
                PathBuf::from(directory)
            } else {
                let persisted = state.app.state.read().await;
                let name = filename
                    .clone()
                    .or_else(|| magnet_display_name(url))
                    .unwrap_or_else(|| filename_from_url(url));
                let route = match_rule(
                    &persisted.default_download_dir,
                    &persisted.download_rules,
                    &name,
                )?;
                expand_peer_home(&route.rule.directory, peer.uid)?
            };
            validate_destination(&destination, peer)?;
            *directory = Some(destination.display().to_string());
        }
        ApiRequest::SetDownloadRouting {
            default_download_dir,
            rules,
        } => {
            let expanded_default = expand_peer_home(default_download_dir, peer.uid)?;
            validate_destination(&expanded_default, peer)?;
            *default_download_dir = expanded_default.display().to_string();
            for rule in rules {
                let expanded = expand_peer_home(&rule.directory, peer.uid)?;
                validate_destination(&expanded, peer)?;
                rule.directory = expanded.display().to_string();
            }
        }
        ApiRequest::Cancel {
            gid,
            delete_files: true,
        } => {
            let snapshot = state.snapshot().await;
            let item = snapshot
                .current_downloads
                .iter()
                .chain(snapshot.history_downloads.iter())
                .find(|item| &item.gid == gid)
                .ok_or_else(|| eyre!("download_not_found: {gid}"))?;
            if let Some(path) = &item.primary_path {
                validate_destination(Path::new(path), peer)?;
            }
            if let Some(torrent) = snapshot
                .torrents
                .downloads
                .iter()
                .find(|item| &item.gid == gid)
            {
                validate_destination(Path::new(&torrent.output_folder), peer)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn expand_peer_home(input: &str, uid: u32) -> Result<PathBuf> {
    if let Some(rest) = input.strip_prefix("~/") {
        return Ok(home_for_uid(uid)
            .ok_or_else(|| eyre!("path_not_permitted: cannot determine home for uid {uid}"))?
            .join(rest));
    }
    Ok(expand_home(input))
}

pub fn validate_destination(path: &Path, peer: PeerIdentity) -> Result<()> {
    if !path.is_absolute() {
        bail!(
            "path_not_permitted: destination must be absolute when using a system daemon: {}",
            path.display()
        );
    }
    let normalized = normalize(path)?;
    let home = home_for_uid(peer.uid).ok_or_else(|| {
        eyre!(
            "path_not_permitted: cannot determine home directory for uid {}",
            peer.uid
        )
    })?;
    let home = home.canonicalize().unwrap_or(home);
    if normalized.starts_with(&home) {
        return Ok(());
    }
    let groups = peer_groups(peer);
    let mut existing = normalized.as_path();
    while !existing.exists() {
        existing = existing.parent().ok_or_else(|| {
            eyre!(
                "path_not_permitted: no existing ancestor for {}",
                normalized.display()
            )
        })?;
    }
    let meta = fs::metadata(existing)?;
    if !meta.is_dir() {
        bail!(
            "path_not_permitted: {} is not a directory",
            existing.display()
        );
    }
    if !can_traverse(existing, peer.uid, &groups) {
        bail!(
            "path_not_permitted: uid {} cannot traverse to {}",
            peer.uid,
            existing.display()
        );
    }
    if allowed(&meta, peer.uid, &groups, true, true) {
        Ok(())
    } else {
        bail!(
            "path_not_permitted: uid {} cannot create downloads in {}",
            peer.uid,
            existing.display()
        )
    }
}

fn can_traverse(path: &Path, uid: u32, groups: &HashSet<u32>) -> bool {
    let mut current = PathBuf::from("/");
    for component in path.components().skip(1) {
        current.push(component.as_os_str());
        let Ok(meta) = fs::metadata(&current) else {
            return false;
        };
        if meta.is_dir() && !allowed(&meta, uid, groups, false, true) {
            return false;
        }
    }
    true
}

fn normalize(path: &Path) -> Result<PathBuf> {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::RootDir => out.push("/"),
            Component::Normal(v) => out.push(v),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    bail!("path_not_permitted: invalid parent traversal")
                }
            }
            Component::Prefix(_) => bail!("path_not_permitted: unsupported path prefix"),
        }
    }
    let mut existing = out.as_path();
    let mut tail = Vec::new();
    while !existing.exists() {
        let name = existing
            .file_name()
            .ok_or_else(|| eyre!("path_not_permitted: invalid path"))?;
        tail.push(name.to_os_string());
        existing = existing
            .parent()
            .ok_or_else(|| eyre!("path_not_permitted: invalid path"))?;
    }
    let mut canonical = existing.canonicalize()?;
    for part in tail.into_iter().rev() {
        canonical.push(part)
    }
    Ok(canonical)
}

fn home_for_uid(uid: u32) -> Option<PathBuf> {
    fs::read_to_string("/etc/passwd")
        .ok()?
        .lines()
        .find_map(|line| {
            let p: Vec<_> = line.split(':').collect();
            if p.len() > 5 && p[2].parse::<u32>().ok() == Some(uid) {
                Some(PathBuf::from(p[5]))
            } else {
                None
            }
        })
}
fn peer_groups(peer: PeerIdentity) -> HashSet<u32> {
    let mut groups = HashSet::from([peer.gid]);
    if let Some(pid) = peer.pid
        && let Ok(status) = fs::read_to_string(format!("/proc/{pid}/status"))
        && let Some(line) = status.lines().find(|l| l.starts_with("Groups:"))
    {
        groups.extend(
            line[7..]
                .split_whitespace()
                .filter_map(|v| v.parse::<u32>().ok()),
        );
    }
    groups
}
fn allowed(
    meta: &fs::Metadata,
    uid: u32,
    groups: &HashSet<u32>,
    write: bool,
    execute: bool,
) -> bool {
    let mode = meta.mode();
    let bits = if meta.uid() == uid {
        (mode >> 6) & 7
    } else if groups.contains(&meta.gid()) {
        (mode >> 3) & 7
    } else {
        mode & 7
    };
    (!write || bits & 2 != 0) && (!execute || bits & 1 != 0)
}
fn filename_from_url(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|u| {
            u.path_segments()
                .and_then(|mut s| s.next_back())
                .map(str::to_string)
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "download".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn own_home_is_allowed() {
        let uid = unsafe { libc::geteuid() };
        let gid = unsafe { libc::getegid() };
        let home = home_for_uid(uid).unwrap();
        validate_destination(
            &home.join("Downloads/new"),
            PeerIdentity {
                uid,
                gid,
                pid: Some(std::process::id()),
            },
        )
        .unwrap()
    }
    #[test]
    fn parent_escape_normalizes() {
        assert_eq!(
            normalize(Path::new("/tmp/a/../b")).unwrap(),
            PathBuf::from("/tmp/b")
        )
    }
}
