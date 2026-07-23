use std::{
    collections::HashSet,
    num::NonZeroU32,
    path::PathBuf,
    sync::{Arc, RwLock},
};

use color_eyre::eyre::{Context, Result, eyre};
use librqbit::{
    AddTorrent, AddTorrentOptions, Api, ManagedTorrent, Session, SessionOptions,
    SessionPersistenceConfig, TorrentStatsState,
    api::{ApiTorrentListOpts, TorrentIdOrHash},
    limits::LimitsConfig,
};
use tokio::io::AsyncReadExt;

use crate::{
    daemon::{
        DownloadItem, DownloadStatus,
        snapshot::{TorrentDownloadSnapshot, TorrentFileSnapshot},
    },
    paths::AppPaths,
};

const GID_PREFIX: &str = "torrent:";
const PIECE_MAP_LIMIT: usize = 160;

pub struct TorrentEngine {
    session: Arc<Session>,
    api: Api,
    sequential_torrents: RwLock<HashSet<String>>,
}

impl std::fmt::Debug for TorrentEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TorrentEngine").finish_non_exhaustive()
    }
}

impl TorrentEngine {
    pub async fn new(paths: &AppPaths, default_output_folder: PathBuf) -> Result<Self> {
        paths.ensure_dirs()?;
        tokio::fs::create_dir_all(&paths.torrent_session_dir)
            .await
            .wrap_err_with(|| {
                format!(
                    "failed to create torrent session directory {}",
                    paths.torrent_session_dir.display()
                )
            })?;
        tokio::fs::create_dir_all(&default_output_folder)
            .await
            .wrap_err_with(|| {
                format!(
                    "failed to create torrent download directory {}",
                    default_output_folder.display()
                )
            })?;

        let session = Session::new_with_opts(
            default_output_folder,
            SessionOptions {
                fastresume: true,
                persistence: Some(SessionPersistenceConfig::Json {
                    folder: Some(paths.torrent_session_dir.clone()),
                }),
                disable_dht_persistence: true,
                listen_port_range: Some(49000..49100),
                ..SessionOptions::default()
            },
        )
        .await
        .map_err(|error| eyre!("failed to start torrent engine: {error:#}"))?;
        let api = Api::new(session.clone(), None);
        Ok(Self {
            session,
            api,
            sequential_torrents: RwLock::new(HashSet::new()),
        })
    }

    pub fn gid(id: usize) -> String {
        format!("{GID_PREFIX}{id}")
    }

    pub fn parse_gid(gid: &str) -> Option<usize> {
        gid.strip_prefix(GID_PREFIX)?.parse().ok()
    }

    pub fn is_torrent_gid(gid: &str) -> bool {
        gid.starts_with(GID_PREFIX)
    }

    pub async fn add_url(
        &self,
        url: &str,
        output_folder: PathBuf,
        limit_bps: Option<u64>,
        sequential: bool,
    ) -> Result<String> {
        tokio::fs::create_dir_all(&output_folder)
            .await
            .wrap_err_with(|| format!("failed to create {}", output_folder.display()))?;
        let response = self
            .session
            .add_torrent(
                AddTorrent::from_url(url),
                Some(AddTorrentOptions {
                    overwrite: true,
                    output_folder: Some(output_folder.display().to_string()),
                    ratelimits: LimitsConfig {
                        download_bps: limit_bps.and_then(nonzero_u32),
                        upload_bps: None,
                    },
                    ..AddTorrentOptions::default()
                }),
            )
            .await
            .map_err(|error| eyre!("failed to add torrent: {error:#}"))?;
        let handle = response
            .into_handle()
            .ok_or_else(|| eyre!("torrent was not added"))?;
        if sequential {
            self.sequential_torrents
                .write()
                .expect("sequential torrent lock poisoned")
                .insert(handle.info_hash().as_string());
            self.spawn_sequential_reader(handle.clone());
        }
        Ok(Self::gid(handle.id()))
    }

    pub async fn add_bytes(
        &self,
        bytes: Vec<u8>,
        output_folder: PathBuf,
        limit_bps: Option<u64>,
        sequential: bool,
    ) -> Result<String> {
        tokio::fs::create_dir_all(&output_folder)
            .await
            .wrap_err_with(|| format!("failed to create {}", output_folder.display()))?;
        let response = self
            .session
            .add_torrent(
                AddTorrent::from_bytes(bytes),
                Some(AddTorrentOptions {
                    overwrite: true,
                    output_folder: Some(output_folder.display().to_string()),
                    ratelimits: LimitsConfig {
                        download_bps: limit_bps.and_then(nonzero_u32),
                        upload_bps: None,
                    },
                    ..AddTorrentOptions::default()
                }),
            )
            .await
            .map_err(|error| eyre!("failed to add torrent: {error:#}"))?;
        let handle = response
            .into_handle()
            .ok_or_else(|| eyre!("torrent was not added"))?;
        if sequential {
            self.sequential_torrents
                .write()
                .expect("sequential torrent lock poisoned")
                .insert(handle.info_hash().as_string());
            self.spawn_sequential_reader(handle.clone());
        }
        Ok(Self::gid(handle.id()))
    }

    pub async fn pause(&self, gid: &str) -> Result<()> {
        let handle = self.handle_from_gid(gid)?;
        self.session
            .pause(&handle)
            .await
            .map_err(|error| eyre!("failed to pause torrent: {error:#}"))
    }

    pub async fn resume(&self, gid: &str) -> Result<()> {
        let handle = self.handle_from_gid(gid)?;
        self.session
            .unpause(&handle)
            .await
            .map_err(|error| eyre!("failed to resume torrent: {error:#}"))
    }

    pub async fn cancel(&self, gid: &str, delete_files: bool) -> Result<()> {
        let id = Self::parse_gid(gid).ok_or_else(|| eyre!("invalid torrent gid: {gid}"))?;
        self.session
            .delete(TorrentIdOrHash::Id(id), delete_files)
            .await
            .map_err(|error| eyre!("failed to remove torrent: {error:#}"))
    }

    pub fn apply_download_limit(&self, limit_bps: Option<u64>) {
        self.session
            .ratelimits
            .set_download_bps(limit_bps.and_then(nonzero_u32));
    }

    pub fn set_sequential_by_hash(&self, hash: &str, enabled: bool) {
        if enabled {
            self.sequential_torrents
                .write()
                .expect("sequential torrent lock poisoned")
                .insert(hash.to_string());
            if let Some(handle) = self.session.with_torrents(|torrents| {
                for (_, handle) in torrents {
                    if handle.info_hash().as_string().eq_ignore_ascii_case(hash) {
                        return Some(handle.clone());
                    }
                }
                None
            }) {
                self.spawn_sequential_reader(handle);
            }
        } else {
            self.sequential_torrents
                .write()
                .expect("sequential torrent lock poisoned")
                .remove(hash);
        }
    }

    pub fn snapshot(&self) -> Vec<TorrentDownloadSnapshot> {
        self.api
            .api_torrent_list_ext(ApiTorrentListOpts { with_stats: true })
            .torrents
            .into_iter()
            .filter_map(|torrent| {
                let id = torrent.id?;
                let stats = torrent.stats?;
                let details = self.api.api_torrent_details(TorrentIdOrHash::Id(id)).ok();
                let live = self
                    .session
                    .get(TorrentIdOrHash::Id(id))
                    .and_then(|handle| handle.live());
                let peers = live.as_ref().map(|live| live.stats_snapshot().peer_stats);
                let peer_ips = live
                    .as_ref()
                    .map(|live| {
                        let mut ips = live
                            .per_peer_stats_snapshot(Default::default())
                            .peers
                            .keys()
                            .cloned()
                            .collect::<Vec<_>>();
                        ips.sort();
                        ips
                    })
                    .unwrap_or_default();
                let piece_map = self
                    .api
                    .api_dump_haves(TorrentIdOrHash::Id(id))
                    .map(|value| truncate_piece_map(&value))
                    .unwrap_or_else(|_| "-".into());
                let files = details
                    .as_ref()
                    .and_then(|details| details.files.as_ref())
                    .map(|files| {
                        files
                            .iter()
                            .enumerate()
                            .map(|(idx, file)| TorrentFileSnapshot {
                                name: file.name.clone(),
                                length: file.length,
                                completed_bytes: stats.file_progress.get(idx).copied().unwrap_or(0),
                                included: file.included,
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let sequential_download = self
                    .sequential_torrents
                    .read()
                    .expect("sequential torrent lock poisoned")
                    .contains(torrent.info_hash.as_str());
                Some(TorrentDownloadSnapshot {
                    gid: Self::gid(id),
                    id,
                    name: torrent
                        .name
                        .clone()
                        .unwrap_or_else(|| torrent.info_hash.clone()),
                    info_hash: torrent.info_hash,
                    output_folder: torrent.output_folder,
                    status: status_from_stats(stats.state, stats.finished),
                    total_bytes: stats.total_bytes,
                    completed_bytes: stats.progress_bytes,
                    download_speed_bps: stats
                        .live
                        .as_ref()
                        .map(|live| speed_to_bps(live.download_speed.mbps))
                        .unwrap_or(0),
                    upload_speed_bps: stats
                        .live
                        .as_ref()
                        .map(|live| speed_to_bps(live.upload_speed.mbps))
                        .unwrap_or(0),
                    live_peers: peers.as_ref().map(|peers| peers.live).unwrap_or(0),
                    seen_peers: peers.as_ref().map(|peers| peers.seen).unwrap_or(0),
                    queued_peers: peers
                        .as_ref()
                        .map(|peers| peers.queued + peers.connecting)
                        .unwrap_or(0),
                    peer_ips,
                    piece_map,
                    sequential_download,
                    files,
                })
            })
            .collect()
    }

    pub fn current_download_items(items: &[TorrentDownloadSnapshot]) -> Vec<DownloadItem> {
        items
            .iter()
            .filter(|item| {
                !matches!(
                    item.status,
                    DownloadStatus::Complete | DownloadStatus::Error
                )
            })
            .cloned()
            .map(download_item_from_torrent)
            .collect()
    }

    pub fn terminal_download_items(items: &[TorrentDownloadSnapshot]) -> Vec<DownloadItem> {
        items
            .iter()
            .filter(|item| {
                matches!(
                    item.status,
                    DownloadStatus::Complete | DownloadStatus::Error
                )
            })
            .cloned()
            .map(download_item_from_torrent)
            .collect()
    }

    fn handle_from_gid(&self, gid: &str) -> Result<Arc<ManagedTorrent>> {
        let id = Self::parse_gid(gid).ok_or_else(|| eyre!("invalid torrent gid: {gid}"))?;
        self.session
            .get(TorrentIdOrHash::Id(id))
            .ok_or_else(|| eyre!("torrent not found: {gid}"))
    }

    fn spawn_sequential_reader(&self, handle: Arc<ManagedTorrent>) {
        tokio::spawn(async move {
            if handle.wait_until_initialized().await.is_err() {
                return;
            }
            let file_count = handle
                .with_metadata(|metadata| metadata.file_infos.len())
                .unwrap_or(0);
            for file_id in 0..file_count {
                let Ok(mut stream) = handle.clone().stream(file_id) else {
                    continue;
                };
                let mut buf = [0u8; 256 * 1024];
                loop {
                    match stream.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
            }
        });
    }
}

fn download_item_from_torrent(item: TorrentDownloadSnapshot) -> DownloadItem {
    DownloadItem {
        gid: item.gid,
        status: item.status,
        name: item.name,
        primary_path: Some(item.output_folder),
        source_uri: Some(format!("magnet:?xt=urn:btih:{}", item.info_hash)),
        info_hash: Some(item.info_hash),
        num_seeders: Some(item.live_peers as u32),
        followed_by: Vec::new(),
        belongs_to: None,
        is_metadata_only: false,
        total_bytes: item.total_bytes,
        completed_bytes: item.completed_bytes,
        download_speed_bps: item.download_speed_bps,
        realtime_download_speed_bps: item.download_speed_bps,
        upload_speed_bps: item.upload_speed_bps,
        eta_seconds: torrent_eta(
            item.total_bytes,
            item.completed_bytes,
            item.download_speed_bps,
        ),
        connections: Some(item.live_peers as u32),
        error_code: None,
        error_message: None,
    }
}

fn status_from_stats(state: TorrentStatsState, finished: bool) -> DownloadStatus {
    if finished {
        return DownloadStatus::Complete;
    }
    match state {
        TorrentStatsState::Initializing | TorrentStatsState::Live => DownloadStatus::Active,
        TorrentStatsState::Paused => DownloadStatus::Paused,
        TorrentStatsState::Error => DownloadStatus::Error,
    }
}

fn torrent_eta(total_bytes: u64, completed_bytes: u64, speed_bps: u64) -> Option<u64> {
    if speed_bps > 0 && total_bytes >= completed_bytes {
        Some((total_bytes - completed_bytes) / speed_bps.max(1))
    } else {
        None
    }
}

fn nonzero_u32(value: u64) -> Option<NonZeroU32> {
    NonZeroU32::new(value.min(u32::MAX as u64) as u32)
}

fn speed_to_bps(mib_per_sec: f64) -> u64 {
    (mib_per_sec * 1024.0 * 1024.0).max(0.0) as u64
}

fn truncate_piece_map(value: &str) -> String {
    if value.len() <= PIECE_MAP_LIMIT {
        return value.to_string();
    }
    format!("{}...", &value[..PIECE_MAP_LIMIT])
}
