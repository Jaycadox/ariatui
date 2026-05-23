use std::fmt::Write as _;

use axum::{
    Form, Json, Router,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use axum_extra::extract::{
    CookieJar,
    cookie::{Cookie, SameSite},
};
use chrono::{Duration, Local};
use color_eyre::eyre::Result;
use serde::{Deserialize, Serialize};
use time::Duration as CookieDuration;
use url::form_urlencoded;

use crate::{
    daemon::{
        ApiPayload, ApiRequest, DownloadItem, DownloadStatus, ResolvedHttpUrl, SharedDaemonState,
        Snapshot,
    },
    download_uri::{DownloadUriKind, classify_download_uri, magnet_display_name},
    eta::{ProjectionPhaseEnd, ScheduledEtaPhase, ScheduledEtaProjection, project_scheduled_eta},
    list_view::{
        CurrentFilter, CurrentSort, HistoryFilter, HistorySort, current_visible_items,
        history_visible_items,
    },
    routing::{DownloadRoutingRule, describe_directory_input, match_rule, validate_rule},
    state::{
        CancelBehaviorPreference, ManualOrScheduled, TorrentStreamingMode,
        validate_torrent_size_mib,
    },
    units::{self, Percentage, format_bytes, format_bytes_per_sec, format_eta, format_limit},
    web::{
        AUTH_COOKIE_NAME, PAIR_COOKIE_NAME, PAIRING_TTL_SECS, PairingStatus, create_or_get_pairing,
        pairing_status, revoke_session, session_is_valid, token_expires_in_secs,
        validate_bind_address, validate_cookie_days,
    },
    webhook::{WebhookPingMode, validate_discord_webhook_url, validate_ping_id},
};

pub fn router(state: SharedDaemonState) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/login", get(login_page))
        .route("/login/status", get(login_status))
        .route("/logout", post(logout))
        .route(
            "/extension/add",
            get(extension_add_page).post(extension_add_submit),
        )
        .route("/api/pairings", post(api_create_pairing))
        .route("/api/pairings/{request_id}", get(api_pairing_status))
        .route("/api/session", get(api_session).delete(api_delete_session))
        .route("/api/downloads", post(api_add_download))
        .route("/current", get(current_page))
        .route("/current/pause-all", post(pause_all_downloads))
        .route("/current/resume-all", post(resume_all_downloads))
        .route("/current/add", get(add_url_page))
        .route("/current/add/resolve", post(add_url_resolve))
        .route("/current/add/confirm", post(add_url_confirm))
        .route("/current/{gid}/move/up", post(move_download_up))
        .route("/current/{gid}/move/down", post(move_download_down))
        .route("/current/{gid}/reorder", post(reorder_download))
        .route("/current/{gid}/pause", post(pause_download))
        .route("/current/{gid}/resume", post(resume_download))
        .route(
            "/current/{gid}/cancel",
            get(cancel_page).post(cancel_submit),
        )
        .route("/history", get(history_page))
        .route("/history/purge", post(purge_history))
        .route("/history/{gid}/remove", post(remove_history))
        .route("/scheduler", get(scheduler_page))
        .route(
            "/scheduler/manual",
            get(edit_manual_page).post(save_manual_limit),
        )
        .route(
            "/scheduler/usual",
            get(edit_usual_page).post(save_usual_limit),
        )
        .route("/scheduler/range/new", get(new_range_page))
        .route("/scheduler/range/{start}/{end}/edit", get(edit_range_page))
        .route("/scheduler/range/save", post(save_range))
        .route("/scheduler/range/delete", post(delete_range))
        .route("/scheduler/mode", post(set_scheduler_mode))
        .route("/scheduler/quick", post(set_quick_speed_mode))
        .route("/torrents", get(torrents_page).post(save_torrents))
        .route("/routing", get(routing_page))
        .route("/routing/rule/new", get(new_rule_page))
        .route("/routing/rule/{index}/edit", get(edit_rule_page))
        .route("/routing/rule/save", post(save_rule))
        .route("/routing/rule/{index}/delete", post(delete_rule))
        .route("/routing/rule/{index}/move/up", post(move_rule_up))
        .route("/routing/rule/{index}/move/down", post(move_rule_down))
        .route("/routing/rule/reorder", post(reorder_rule))
        .route("/webhooks", get(webhooks_page).post(save_webhooks))
        .route("/webhooks/test", post(trigger_webhook_test))
        .route("/web-ui", get(web_ui_page).post(save_web_ui))
        .with_state(state)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WebTab {
    Current,
    History,
    Scheduler,
    Torrents,
    Routing,
    Webhooks,
    WebUi,
}

impl WebTab {
    fn title(self) -> &'static str {
        match self {
            Self::Current => "Downloads",
            Self::History => "Activity",
            Self::Scheduler => "Speed",
            Self::Torrents => "Torrents",
            Self::Routing => "Folders",
            Self::Webhooks => "Alerts",
            Self::WebUi => "Access",
        }
    }

    fn href(self) -> &'static str {
        match self {
            Self::Current => "/current",
            Self::History => "/history",
            Self::Scheduler => "/scheduler",
            Self::Torrents => "/torrents",
            Self::Routing => "/routing",
            Self::Webhooks => "/webhooks",
            Self::WebUi => "/web-ui",
        }
    }

    fn all() -> [Self; 7] {
        [
            Self::Current,
            Self::History,
            Self::Scheduler,
            Self::Torrents,
            Self::Routing,
            Self::Webhooks,
            Self::WebUi,
        ]
    }
}

#[derive(Debug, Deserialize, Default)]
struct ItemQuery {
    selected: Option<String>,
    test: Option<String>,
    search: Option<String>,
    filter: Option<String>,
    sort: Option<String>,
}

#[derive(Debug, Deserialize)]
struct UrlFormData {
    url: String,
}

#[derive(Debug, Deserialize)]
struct ConfirmAddFormData {
    url: String,
    filename_choice: String,
    custom_filename: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CancelFormData {
    delete_files: bool,
    remember_behavior: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LimitFormData {
    value: String,
}

#[derive(Debug, Deserialize)]
struct ModeFormData {
    mode: String,
}

#[derive(Debug, Deserialize)]
struct QuickSpeedFormData {
    action: String,
    return_to: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RangeFormData {
    start_hour: usize,
    end_hour: usize,
    limit: String,
}

#[derive(Debug, Deserialize)]
struct ReorderFormData {
    source: Option<String>,
    target: String,
    position: String,
}

#[derive(Debug, Deserialize)]
struct RoutingRuleFormData {
    index: Option<usize>,
    pattern: String,
    directory: String,
}

#[derive(Debug, Deserialize)]
struct WebhookFormData {
    discord_webhook_url: String,
    ping_mode: String,
    ping_id: String,
}

#[derive(Debug, Deserialize)]
struct WebUiFormData {
    enabled: Option<String>,
    bind_address: String,
    port: u16,
    cookie_days: u32,
}

#[derive(Debug, Deserialize)]
struct ApiAddDownloadBody {
    url: String,
    filename: Option<String>,
}

#[derive(Debug, Deserialize)]
struct LoginQuery {
    next: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ExtensionAddQuery {
    url: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ExtensionAddFormData {
    url: String,
    filename_choice: String,
    custom_filename: Option<String>,
}

#[derive(Debug, Deserialize)]
struct TorrentSettingsFormData {
    mode: String,
    head_size_mib: u32,
    tail_size_mib: u32,
}

#[derive(Debug, Deserialize)]
struct RangePath {
    start: usize,
    end: usize,
}

#[derive(Debug, Deserialize)]
struct RulePath {
    index: usize,
}

#[derive(Debug, Serialize)]
struct LoginStatusBody {
    status: &'static str,
}

#[derive(Debug, Serialize)]
struct ApiErrorBody {
    error: String,
}

#[derive(Debug)]
struct ApiErrorResponse {
    status: StatusCode,
    message: String,
}

impl ApiErrorResponse {
    fn unauthorized(message: &str) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: message.to_string(),
        }
    }

    fn bad_request(message: &str) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: message.to_string(),
        }
    }

    fn internal(message: &str) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: message.to_string(),
        }
    }
}

impl IntoResponse for ApiErrorResponse {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(ApiErrorBody {
                error: self.message,
            }),
        )
            .into_response()
    }
}

#[derive(Debug, Serialize)]
struct ApiCreatePairingBody {
    request_id: String,
    pin: String,
    expires_in_secs: u64,
}

#[derive(Debug, Serialize)]
struct ApiPairingPendingBody {
    status: &'static str,
}

#[derive(Debug, Serialize)]
struct ApiPairingApprovedBody {
    status: &'static str,
    auth_token: String,
    expires_in_secs: u64,
}

#[derive(Debug, Serialize)]
struct ApiAddDownloadQueuedResponse {
    status: &'static str,
    queued: bool,
    display_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    final_url: Option<String>,
}

#[derive(Debug, Serialize)]
struct ApiAddDownloadPromptResponse {
    status: &'static str,
    url: String,
    url_filename: String,
    remote_label: &'static str,
    remote_filename: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    final_url: Option<String>,
}

#[derive(Debug, Clone)]
struct CurrentListQuery {
    selected: Option<String>,
    search: String,
    filter: CurrentFilter,
    sort: CurrentSort,
}

impl CurrentListQuery {
    fn from_query(query: &ItemQuery) -> Self {
        Self {
            selected: query.selected.clone(),
            search: query.search.clone().unwrap_or_default().trim().to_string(),
            filter: CurrentFilter::from_query(query.filter.as_deref().unwrap_or_default()),
            sort: CurrentSort::from_query(query.sort.as_deref().unwrap_or_default()),
        }
    }
}

#[derive(Debug, Clone)]
struct HistoryListQuery {
    selected: Option<String>,
    search: String,
    filter: HistoryFilter,
    sort: HistorySort,
}

impl HistoryListQuery {
    fn from_query(query: &ItemQuery) -> Self {
        Self {
            selected: query.selected.clone(),
            search: query.search.clone().unwrap_or_default().trim().to_string(),
            filter: HistoryFilter::from_query(query.filter.as_deref().unwrap_or_default()),
            sort: HistorySort::from_query(query.sort.as_deref().unwrap_or_default()),
        }
    }
}

async fn root(
    State(state): State<SharedDaemonState>,
    jar: CookieJar,
    Query(query): Query<LoginQuery>,
) -> Response {
    if authenticated(&state, &jar).await.unwrap_or(false) {
        Redirect::to(&root_next_path(query.next.as_deref())).into_response()
    } else {
        Redirect::to(&login_path(query.next.as_deref())).into_response()
    }
}

async fn login_page(
    State(state): State<SharedDaemonState>,
    jar: CookieJar,
    Query(query): Query<LoginQuery>,
) -> Response {
    let next = login_success_path(query.next.as_deref());
    if authenticated(&state, &jar).await.unwrap_or(false) {
        return Redirect::to(&next).into_response();
    }
    match create_or_get_pairing(
        state.as_ref(),
        jar.get(PAIR_COOKIE_NAME).map(|cookie| cookie.value()),
    )
    .await
    {
        Ok((request_id, pin)) => {
            let cookie = Cookie::build((PAIR_COOKIE_NAME, request_id))
                .path("/")
                .http_only(true)
                .same_site(SameSite::Strict)
                .max_age(CookieDuration::minutes(5))
                .build();
            (jar.add(cookie), Html(render_login(&pin, &next))).into_response()
        }
        Err(error) => Html(render_public_shell(
            "Login",
            &format!("<p class=\"error\">{}</p>", esc(&error.to_string())),
            Some(&next),
        ))
        .into_response(),
    }
}

async fn login_status(State(state): State<SharedDaemonState>, jar: CookieJar) -> Response {
    let Some(pair_cookie) = jar.get(PAIR_COOKIE_NAME) else {
        return Json(LoginStatusBody { status: "expired" }).into_response();
    };
    match pairing_status(state.as_ref(), pair_cookie.value()).await {
        Ok(PairingStatus::Pending) => Json(LoginStatusBody { status: "pending" }).into_response(),
        Ok(PairingStatus::Expired) => Json(LoginStatusBody { status: "expired" }).into_response(),
        Ok(PairingStatus::Approved { auth_token }) => {
            let persisted = state.app.state.read().await.clone();
            let auth_cookie = Cookie::build((AUTH_COOKIE_NAME, auth_token))
                .path("/")
                .http_only(true)
                .same_site(SameSite::Strict)
                .max_age(CookieDuration::days(persisted.web_ui_cookie_days as i64))
                .build();
            let pair_cookie = Cookie::build((PAIR_COOKIE_NAME, ""))
                .path("/")
                .http_only(true)
                .same_site(SameSite::Strict)
                .max_age(CookieDuration::seconds(0))
                .build();
            (
                jar.add(auth_cookie).remove(pair_cookie),
                Json(LoginStatusBody { status: "approved" }),
            )
                .into_response()
        }
        Err(error) => Html(render_public_shell(
            "Login",
            &format!("<p class=\"error\">{}</p>", esc(&error.to_string())),
            Some("/current"),
        ))
        .into_response(),
    }
}

async fn api_create_pairing(State(state): State<SharedDaemonState>) -> Response {
    match create_or_get_pairing(state.as_ref(), None).await {
        Ok((request_id, pin)) => Json(ApiCreatePairingBody {
            request_id,
            pin,
            expires_in_secs: PAIRING_TTL_SECS,
        })
        .into_response(),
        Err(error) => ApiErrorResponse::internal(&error.to_string()).into_response(),
    }
}

async fn api_pairing_status(
    State(state): State<SharedDaemonState>,
    Path(request_id): Path<String>,
) -> Response {
    match pairing_status(state.as_ref(), &request_id).await {
        Ok(PairingStatus::Pending) => {
            Json(ApiPairingPendingBody { status: "pending" }).into_response()
        }
        Ok(PairingStatus::Expired) => {
            Json(ApiPairingPendingBody { status: "expired" }).into_response()
        }
        Ok(PairingStatus::Approved { auth_token }) => Json(ApiPairingApprovedBody {
            status: "approved",
            expires_in_secs: token_expires_in_secs(&auth_token).unwrap_or_default(),
            auth_token,
        })
        .into_response(),
        Err(error) => ApiErrorResponse::internal(&error.to_string()).into_response(),
    }
}

async fn api_session(
    State(state): State<SharedDaemonState>,
    headers: HeaderMap,
    jar: CookieJar,
) -> Response {
    match authenticated_api_token(&state, &headers, &jar).await {
        Some(_) => StatusCode::NO_CONTENT.into_response(),
        None => ApiErrorResponse::unauthorized("authentication required").into_response(),
    }
}

async fn api_delete_session(
    State(state): State<SharedDaemonState>,
    headers: HeaderMap,
    jar: CookieJar,
) -> Response {
    match authenticated_api_token(&state, &headers, &jar).await {
        Some(token) => {
            revoke_session(state.as_ref(), &token).await;
            StatusCode::NO_CONTENT.into_response()
        }
        None => ApiErrorResponse::unauthorized("authentication required").into_response(),
    }
}

async fn api_add_download(
    State(state): State<SharedDaemonState>,
    headers: HeaderMap,
    jar: CookieJar,
    Json(body): Json<ApiAddDownloadBody>,
) -> Response {
    if authenticated_api_token(&state, &headers, &jar)
        .await
        .is_none()
    {
        return ApiErrorResponse::unauthorized("authentication required").into_response();
    }
    match prepare_download_submission(&state, &body.url).await {
        Ok(prepared) => {
            if body.filename.is_none()
                && let PreparedDownloadSubmission::Prompt {
                    url,
                    url_filename,
                    remote_label,
                    remote_filename,
                    final_url,
                } = &prepared
            {
                return Json(ApiAddDownloadPromptResponse {
                    status: "needs_filename",
                    url: url.clone(),
                    url_filename: url_filename.clone(),
                    remote_label,
                    remote_filename: remote_filename.clone(),
                    final_url: final_url.clone(),
                })
                .into_response();
            }
            let queued = match prepared.into_queue_with_filename(body.filename) {
                Ok(queued) => queued,
                Err(error) => return error.into_response(),
            };
            let response = ApiAddDownloadQueuedResponse {
                status: "queued",
                queued: true,
                display_name: queued.display_name.clone(),
                final_url: queued.final_url.clone(),
            };
            match state
                .execute(ApiRequest::AddHttpUrl {
                    url: queued.url,
                    filename: queued.filename,
                })
                .await
            {
                Ok(_) => (StatusCode::ACCEPTED, Json(response)).into_response(),
                Err(error) => ApiErrorResponse::bad_request(&error.to_string()).into_response(),
            }
        }
        Err(error) => error.into_response(),
    }
}

async fn extension_add_page(
    State(state): State<SharedDaemonState>,
    jar: CookieJar,
    Query(query): Query<ExtensionAddQuery>,
) -> Response {
    let url = query.url.unwrap_or_default().trim().to_string();
    let next = extension_add_path(&url);
    if let Some(response) = auth_redirect_with_next(&state, &jar, Some(&next)).await {
        return response;
    }
    match prepare_download_submission(&state, &url).await {
        Ok(PreparedDownloadSubmission::Prompt {
            url,
            url_filename,
            remote_label,
            remote_filename,
            final_url,
        }) => Html(render_extension_add_prompt(
            &url,
            &url_filename,
            remote_label,
            &remote_filename,
            final_url.as_deref(),
            None,
        ))
        .into_response(),
        Ok(prepared) => {
            let queued = match prepared.into_queue_with_filename(None) {
                Ok(queued) => queued,
                Err(error) => return error.into_response(),
            };
            match state
                .execute(ApiRequest::AddHttpUrl {
                    url: queued.url,
                    filename: queued.filename,
                })
                .await
            {
                Ok(_) => Html(render_extension_add_done(
                    &queued.display_name,
                    queued.final_url.as_deref(),
                ))
                .into_response(),
                Err(error) => Html(render_extension_add_error(&error.to_string())).into_response(),
            }
        }
        Err(error) => Html(render_extension_add_error(&error.message)).into_response(),
    }
}

async fn extension_add_submit(
    State(state): State<SharedDaemonState>,
    jar: CookieJar,
    Form(form): Form<ExtensionAddFormData>,
) -> Response {
    let next = extension_add_path(&form.url);
    if let Some(response) = auth_redirect_with_next(&state, &jar, Some(&next)).await {
        return response;
    }
    let prepared = match prepare_download_submission(&state, &form.url).await {
        Ok(prepared) => prepared,
        Err(error) => return Html(render_extension_add_error(&error.message)).into_response(),
    };
    let requested_filename = if form.filename_choice == "__custom__" {
        form.custom_filename
    } else {
        Some(form.filename_choice)
    };
    let queued = match prepared.into_queue_with_filename(requested_filename) {
        Ok(queued) => queued,
        Err(error) => {
            return Html(render_extension_add_prompt_from_submission(
                &form.url,
                error.message.as_str(),
            ))
            .into_response();
        }
    };
    match state
        .execute(ApiRequest::AddHttpUrl {
            url: queued.url,
            filename: queued.filename,
        })
        .await
    {
        Ok(_) => Html(render_extension_add_done(
            &queued.display_name,
            queued.final_url.as_deref(),
        ))
        .into_response(),
        Err(error) => Html(render_extension_add_prompt_from_submission(
            &form.url,
            &error.to_string(),
        ))
        .into_response(),
    }
}

async fn logout(State(state): State<SharedDaemonState>, jar: CookieJar) -> Response {
    if let Some(cookie) = jar.get(AUTH_COOKIE_NAME) {
        revoke_session(state.as_ref(), cookie.value()).await;
    }
    let cookie = Cookie::build((AUTH_COOKIE_NAME, ""))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Strict)
        .max_age(CookieDuration::seconds(0))
        .build();
    let pair_cookie = Cookie::build((PAIR_COOKIE_NAME, ""))
        .path("/")
        .http_only(true)
        .same_site(SameSite::Strict)
        .max_age(CookieDuration::seconds(0))
        .build();
    (
        jar.remove(cookie).remove(pair_cookie),
        Redirect::to("/login"),
    )
        .into_response()
}

#[derive(Debug, Clone)]
struct QueuedDownload {
    url: String,
    filename: Option<String>,
    display_name: String,
    final_url: Option<String>,
}

#[derive(Debug, Clone)]
enum PreparedDownloadSubmission {
    Queue(QueuedDownload),
    Prompt {
        url: String,
        url_filename: String,
        remote_label: &'static str,
        remote_filename: String,
        final_url: Option<String>,
    },
}

impl PreparedDownloadSubmission {
    fn into_api_queue(self) -> QueuedDownload {
        match self {
            Self::Queue(queue) => queue,
            Self::Prompt {
                url,
                remote_filename,
                final_url,
                ..
            } => QueuedDownload {
                url,
                filename: Some(remote_filename.clone()),
                display_name: remote_filename,
                final_url,
            },
        }
    }

    fn into_queue_with_filename(
        self,
        filename: Option<String>,
    ) -> Result<QueuedDownload, ApiErrorResponse> {
        let custom = filename
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        match self {
            Self::Queue(mut queue) => {
                if let Some(filename) = custom {
                    queue.display_name = filename.clone();
                    queue.filename = Some(filename);
                }
                Ok(queue)
            }
            Self::Prompt { url, final_url, .. } => {
                let Some(filename) = custom else {
                    return Err(ApiErrorResponse::bad_request("filename is required"));
                };
                Ok(QueuedDownload {
                    url,
                    filename: Some(filename.clone()),
                    display_name: filename,
                    final_url,
                })
            }
        }
    }
}

fn bearer_token(headers: &HeaderMap) -> Option<String> {
    let value = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let token = value.strip_prefix("Bearer ")?.trim();
    if token.is_empty() {
        None
    } else {
        Some(token.to_string())
    }
}

async fn authenticated_api_token(
    state: &SharedDaemonState,
    headers: &HeaderMap,
    jar: &CookieJar,
) -> Option<String> {
    if let Some(token) = bearer_token(headers)
        && session_is_valid(state.as_ref(), &token).await
    {
        return Some(token);
    }
    let cookie = jar.get(AUTH_COOKIE_NAME)?;
    if session_is_valid(state.as_ref(), cookie.value()).await {
        Some(cookie.value().to_string())
    } else {
        None
    }
}

fn filename_from_url_fallback(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|parsed| {
            parsed
                .path_segments()
                .and_then(|mut segments| segments.rfind(|segment| !segment.is_empty()))
                .map(str::to_string)
        })
        .filter(|segment| !segment.trim().is_empty())
        .unwrap_or_else(|| "download".into())
}

async fn prepare_download_submission(
    state: &SharedDaemonState,
    url: &str,
) -> Result<PreparedDownloadSubmission, ApiErrorResponse> {
    let url = url.trim().to_string();
    if url.is_empty() {
        return Err(ApiErrorResponse::bad_request("URI cannot be empty"));
    }

    match classify_download_uri(&url)
        .map_err(|error| ApiErrorResponse::bad_request(&error.to_string()))?
    {
        DownloadUriKind::Magnet => Ok(PreparedDownloadSubmission::Queue(QueuedDownload {
            display_name: magnet_display_name(&url).unwrap_or_else(|| "torrent".into()),
            filename: None,
            final_url: None,
            url,
        })),
        DownloadUriKind::HttpLike => match state
            .execute(ApiRequest::ResolveHttpUrl { url: url.clone() })
            .await
        {
            Ok(reply) => match reply.payload {
                Some(ApiPayload::ResolvedHttpUrl(resolved)) => {
                    Ok(prepared_download_from_resolved(resolved))
                }
                _ => Ok(PreparedDownloadSubmission::Queue(QueuedDownload {
                    display_name: filename_from_url_fallback(&url),
                    filename: None,
                    final_url: None,
                    url,
                })),
            },
            Err(_) => Ok(PreparedDownloadSubmission::Queue(QueuedDownload {
                display_name: filename_from_url_fallback(&url),
                filename: None,
                final_url: None,
                url,
            })),
        },
    }
}

fn prepared_download_from_resolved(resolved: ResolvedHttpUrl) -> PreparedDownloadSubmission {
    if resolved.is_torrent {
        return PreparedDownloadSubmission::Queue(QueuedDownload {
            display_name: resolved
                .remote_filename
                .clone()
                .or_else(|| resolved.redirect_filename.clone())
                .unwrap_or_else(|| resolved.url_filename.clone()),
            filename: None,
            final_url: resolved.final_url,
            url: resolved.url,
        });
    }
    if let Some((label, remote_filename)) = prompt_candidate(&resolved) {
        PreparedDownloadSubmission::Prompt {
            url: resolved.url,
            url_filename: resolved.url_filename,
            remote_label: label,
            remote_filename,
            final_url: resolved.final_url,
        }
    } else {
        PreparedDownloadSubmission::Queue(QueuedDownload {
            display_name: resolved.url_filename.clone(),
            filename: Some(resolved.url_filename),
            final_url: resolved.final_url,
            url: resolved.url,
        })
    }
}

async fn current_page(
    State(state): State<SharedDaemonState>,
    jar: CookieJar,
    Query(query): Query<ItemQuery>,
) -> Response {
    if let Some(response) = auth_redirect(&state, &jar).await {
        return response;
    }
    let snapshot = state.snapshot().await;
    let list = CurrentListQuery::from_query(&query);
    Html(render_current_page(&snapshot, &list, None, None, true)).into_response()
}

async fn pause_all_downloads(
    State(state): State<SharedDaemonState>,
    jar: CookieJar,
    Query(query): Query<ItemQuery>,
) -> Response {
    if let Some(response) = auth_redirect(&state, &jar).await {
        return response;
    }
    let _ = state.execute(ApiRequest::PauseAll).await;
    Redirect::to(&current_path(&query)).into_response()
}

async fn resume_all_downloads(
    State(state): State<SharedDaemonState>,
    jar: CookieJar,
    Query(query): Query<ItemQuery>,
) -> Response {
    if let Some(response) = auth_redirect(&state, &jar).await {
        return response;
    }
    let _ = state.execute(ApiRequest::ResumeAll).await;
    Redirect::to(&current_path(&query)).into_response()
}

async fn add_url_page(State(state): State<SharedDaemonState>, jar: CookieJar) -> Response {
    if let Some(response) = auth_redirect(&state, &jar).await {
        return response;
    }
    let snapshot = state.snapshot().await;
    Html(render_add_url_page(&snapshot, None, None, None)).into_response()
}

async fn add_url_resolve(
    State(state): State<SharedDaemonState>,
    jar: CookieJar,
    Form(form): Form<UrlFormData>,
) -> Response {
    if let Some(response) = auth_redirect(&state, &jar).await {
        return response;
    }
    let snapshot = state.snapshot().await;
    let url = form.url.trim().to_string();
    match prepare_download_submission(&state, &url).await {
        Ok(PreparedDownloadSubmission::Prompt {
            url,
            url_filename,
            remote_label,
            remote_filename,
            ..
        }) => Html(render_add_url_page(
            &snapshot,
            None,
            Some((&url, &url_filename, remote_label, &remote_filename)),
            None,
        ))
        .into_response(),
        Ok(prepared) => {
            let queued = prepared.into_api_queue();
            match state
                .execute(ApiRequest::AddHttpUrl {
                    url: queued.url,
                    filename: queued.filename,
                })
                .await
            {
                Ok(_) => Redirect::to("/current").into_response(),
                Err(error) => Html(render_add_url_page(
                    &snapshot,
                    Some(&error.to_string()),
                    None,
                    Some(&url),
                ))
                .into_response(),
            }
        }
        Err(error) => Html(render_add_url_page(
            &snapshot,
            Some(&error.message),
            None,
            Some(&url),
        ))
        .into_response(),
    }
}

async fn add_url_confirm(
    State(state): State<SharedDaemonState>,
    jar: CookieJar,
    Form(form): Form<ConfirmAddFormData>,
) -> Response {
    if let Some(response) = auth_redirect(&state, &jar).await {
        return response;
    }
    let snapshot = state.snapshot().await;
    let filename = if form.filename_choice == "__custom__" {
        form.custom_filename.unwrap_or_default().trim().to_string()
    } else {
        form.filename_choice.trim().to_string()
    };
    if filename.is_empty() {
        return Html(render_add_url_page(
            &snapshot,
            Some("Filename cannot be empty"),
            None,
            Some(&form.url),
        ))
        .into_response();
    }
    match state
        .execute(ApiRequest::AddHttpUrl {
            url: form.url.clone(),
            filename: Some(filename),
        })
        .await
    {
        Ok(_) => Redirect::to("/current").into_response(),
        Err(error) => Html(render_add_url_page(
            &snapshot,
            Some(&error.to_string()),
            None,
            Some(&form.url),
        ))
        .into_response(),
    }
}

async fn pause_download(
    State(state): State<SharedDaemonState>,
    jar: CookieJar,
    Path(gid): Path<String>,
    Query(query): Query<ItemQuery>,
) -> Response {
    if let Some(response) = auth_redirect(&state, &jar).await {
        return response;
    }
    let _ = state.execute(ApiRequest::Pause { gid, force: true }).await;
    Redirect::to(&current_path(&query)).into_response()
}

async fn resume_download(
    State(state): State<SharedDaemonState>,
    jar: CookieJar,
    Path(gid): Path<String>,
    Query(query): Query<ItemQuery>,
) -> Response {
    if let Some(response) = auth_redirect(&state, &jar).await {
        return response;
    }
    let _ = state.execute(ApiRequest::Resume { gid }).await;
    Redirect::to(&current_path(&query)).into_response()
}

async fn move_download_up(
    State(state): State<SharedDaemonState>,
    jar: CookieJar,
    Path(gid): Path<String>,
    Query(query): Query<ItemQuery>,
) -> Response {
    if let Some(response) = auth_redirect(&state, &jar).await {
        return response;
    }
    let _ = state
        .execute(ApiRequest::ChangePosition { gid, offset: -1 })
        .await;
    Redirect::to(&current_path(&query)).into_response()
}

async fn move_download_down(
    State(state): State<SharedDaemonState>,
    jar: CookieJar,
    Path(gid): Path<String>,
    Query(query): Query<ItemQuery>,
) -> Response {
    if let Some(response) = auth_redirect(&state, &jar).await {
        return response;
    }
    let _ = state
        .execute(ApiRequest::ChangePosition { gid, offset: 1 })
        .await;
    Redirect::to(&current_path(&query)).into_response()
}

async fn reorder_download(
    State(state): State<SharedDaemonState>,
    jar: CookieJar,
    Path(gid): Path<String>,
    Query(query): Query<ItemQuery>,
    Form(form): Form<ReorderFormData>,
) -> Response {
    if let Some(response) = auth_redirect(&state, &jar).await {
        return response;
    }
    let snapshot = state.snapshot().await;
    let source = snapshot
        .current_downloads
        .iter()
        .position(|item| item.gid == gid);
    let target = snapshot
        .current_downloads
        .iter()
        .position(|item| item.gid == form.target);
    if let (Some(source), Some(target)) = (source, target)
        && source != target
    {
        let desired = if form.position == "after" {
            target.saturating_add(usize::from(source > target))
        } else {
            target.saturating_sub(usize::from(source < target))
        }
        .min(snapshot.current_downloads.len().saturating_sub(1));
        let offset = desired as i32 - source as i32;
        if offset != 0 {
            let _ = state
                .execute(ApiRequest::ChangePosition { gid, offset })
                .await;
        }
    }
    Redirect::to(&current_path(&query)).into_response()
}

async fn cancel_page(
    State(state): State<SharedDaemonState>,
    jar: CookieJar,
    Path(gid): Path<String>,
) -> Response {
    if let Some(response) = auth_redirect(&state, &jar).await {
        return response;
    }
    let snapshot = state.snapshot().await;
    Html(render_cancel_page(&snapshot, &gid, None)).into_response()
}

async fn cancel_submit(
    State(state): State<SharedDaemonState>,
    jar: CookieJar,
    Path(gid): Path<String>,
    Query(query): Query<ItemQuery>,
    Form(form): Form<CancelFormData>,
) -> Response {
    if let Some(response) = auth_redirect(&state, &jar).await {
        return response;
    }
    let snapshot = state.snapshot().await;
    if let Some(value) = form.remember_behavior.as_deref() {
        let behavior = match value {
            "ask" => Some(CancelBehaviorPreference::Ask),
            "keep_partials" => Some(CancelBehaviorPreference::KeepPartials),
            "delete_partials" => Some(CancelBehaviorPreference::DeletePartials),
            _ => None,
        };
        if let Some(behavior) = behavior {
            let _ = state
                .execute(ApiRequest::SetRememberedCancelBehavior { behavior })
                .await;
        }
    }
    match state
        .execute(ApiRequest::Cancel {
            gid,
            delete_files: form.delete_files,
        })
        .await
    {
        Ok(_) => Redirect::to(&current_path(&query)).into_response(),
        Err(error) => {
            Html(render_cancel_page(&snapshot, "", Some(&error.to_string()))).into_response()
        }
    }
}

async fn history_page(
    State(state): State<SharedDaemonState>,
    jar: CookieJar,
    Query(query): Query<ItemQuery>,
) -> Response {
    if let Some(response) = auth_redirect(&state, &jar).await {
        return response;
    }
    let snapshot = state.snapshot().await;
    let list = HistoryListQuery::from_query(&query);
    Html(render_history_page(&snapshot, &list)).into_response()
}

async fn remove_history(
    State(state): State<SharedDaemonState>,
    jar: CookieJar,
    Path(gid): Path<String>,
    Query(query): Query<ItemQuery>,
) -> Response {
    if let Some(response) = auth_redirect(&state, &jar).await {
        return response;
    }
    let _ = state.execute(ApiRequest::RemoveHistory { gid }).await;
    Redirect::to(&history_path(&query)).into_response()
}

async fn purge_history(
    State(state): State<SharedDaemonState>,
    jar: CookieJar,
    Query(query): Query<ItemQuery>,
) -> Response {
    if let Some(response) = auth_redirect(&state, &jar).await {
        return response;
    }
    let _ = state.execute(ApiRequest::PurgeHistory).await;
    Redirect::to(&history_path(&query)).into_response()
}

async fn scheduler_page(State(state): State<SharedDaemonState>, jar: CookieJar) -> Response {
    if let Some(response) = auth_redirect(&state, &jar).await {
        return response;
    }
    let snapshot = state.snapshot().await;
    Html(render_scheduler_page(&snapshot, None)).into_response()
}

async fn torrents_page(State(state): State<SharedDaemonState>, jar: CookieJar) -> Response {
    if let Some(response) = auth_redirect(&state, &jar).await {
        return response;
    }
    let snapshot = state.snapshot().await;
    Html(render_torrents_page(&snapshot, None)).into_response()
}

async fn save_torrents(
    State(state): State<SharedDaemonState>,
    jar: CookieJar,
    Form(form): Form<TorrentSettingsFormData>,
) -> Response {
    if let Some(response) = auth_redirect(&state, &jar).await {
        return response;
    }
    let snapshot = state.snapshot().await;
    let mode = match form.mode.as_str() {
        "off" => TorrentStreamingMode::Off,
        "start_first" => TorrentStreamingMode::StartFirst,
        "start_and_end_first" => TorrentStreamingMode::StartAndEndFirst,
        _ => {
            return Html(render_torrents_page(
                &snapshot,
                Some("mode must be off, start_first, or start_and_end_first"),
            ))
            .into_response();
        }
    };
    if let Err(error) = validate_torrent_size_mib(form.head_size_mib, "torrent head size") {
        return Html(render_torrents_page(&snapshot, Some(&error.to_string()))).into_response();
    }
    if let Err(error) = validate_torrent_size_mib(form.tail_size_mib, "torrent tail size") {
        return Html(render_torrents_page(&snapshot, Some(&error.to_string()))).into_response();
    }
    match state
        .execute(ApiRequest::SetTorrentStreamingSettings {
            mode,
            head_size_mib: form.head_size_mib,
            tail_size_mib: form.tail_size_mib,
        })
        .await
    {
        Ok(_) => Redirect::to("/torrents").into_response(),
        Err(error) => {
            Html(render_torrents_page(&snapshot, Some(&error.to_string()))).into_response()
        }
    }
}

async fn edit_manual_page(State(state): State<SharedDaemonState>, jar: CookieJar) -> Response {
    if let Some(response) = auth_redirect(&state, &jar).await {
        return response;
    }
    let snapshot = state.snapshot().await;
    Html(render_limit_editor_page(
        &snapshot,
        WebTab::Scheduler,
        "Manual limit",
        "/scheduler/manual",
        &format_limit(snapshot.scheduler.manual_limit_bps),
        "Accepted examples: 10M, 10 mb/s, 10mbps, 1 kbps, unlimited.",
        None,
    ))
    .into_response()
}

async fn save_manual_limit(
    State(state): State<SharedDaemonState>,
    jar: CookieJar,
    Form(form): Form<LimitFormData>,
) -> Response {
    if let Some(response) = auth_redirect(&state, &jar).await {
        return response;
    }
    let snapshot = state.snapshot().await;
    match units::parse_limit(&form.value) {
        Ok(limit_bps) => {
            let _ = state
                .execute(ApiRequest::SetManualLimit { limit_bps })
                .await;
            Redirect::to("/scheduler").into_response()
        }
        Err(error) => Html(render_limit_editor_page(
            &snapshot,
            WebTab::Scheduler,
            "Manual limit",
            "/scheduler/manual",
            &form.value,
            "Accepted examples: 10M, 10 mb/s, 10mbps, 1 kbps, unlimited.",
            Some(&error.to_string()),
        ))
        .into_response(),
    }
}

async fn edit_usual_page(State(state): State<SharedDaemonState>, jar: CookieJar) -> Response {
    if let Some(response) = auth_redirect(&state, &jar).await {
        return response;
    }
    let snapshot = state.snapshot().await;
    Html(render_limit_editor_page(
        &snapshot,
        WebTab::Scheduler,
        "Usual internet speed",
        "/scheduler/usual",
        &format_limit(snapshot.scheduler.usual_internet_speed_bps),
        "This caps scheduled ETA modeling, including unlimited schedule slots.",
        None,
    ))
    .into_response()
}

async fn save_usual_limit(
    State(state): State<SharedDaemonState>,
    jar: CookieJar,
    Form(form): Form<LimitFormData>,
) -> Response {
    if let Some(response) = auth_redirect(&state, &jar).await {
        return response;
    }
    let snapshot = state.snapshot().await;
    match units::parse_limit(&form.value) {
        Ok(limit_bps) => {
            let _ = state
                .execute(ApiRequest::SetUsualInternetSpeed { limit_bps })
                .await;
            Redirect::to("/scheduler").into_response()
        }
        Err(error) => Html(render_limit_editor_page(
            &snapshot,
            WebTab::Scheduler,
            "Usual internet speed",
            "/scheduler/usual",
            &form.value,
            "This caps scheduled ETA modeling, including unlimited schedule slots.",
            Some(&error.to_string()),
        ))
        .into_response(),
    }
}

async fn new_range_page(State(state): State<SharedDaemonState>, jar: CookieJar) -> Response {
    if let Some(response) = auth_redirect(&state, &jar).await {
        return response;
    }
    let snapshot = state.snapshot().await;
    Html(render_range_editor_page(
        &snapshot,
        0,
        24,
        "unlimited",
        None,
    ))
    .into_response()
}

async fn edit_range_page(
    State(state): State<SharedDaemonState>,
    jar: CookieJar,
    Path(path): Path<RangePath>,
) -> Response {
    if let Some(response) = auth_redirect(&state, &jar).await {
        return response;
    }
    let snapshot = state.snapshot().await;
    let limit = snapshot
        .scheduler
        .schedule_limits_bps
        .get(path.start)
        .copied()
        .unwrap_or(None);
    Html(render_range_editor_page(
        &snapshot,
        path.start,
        path.end,
        &format_limit(limit),
        None,
    ))
    .into_response()
}

async fn save_range(
    State(state): State<SharedDaemonState>,
    jar: CookieJar,
    Form(form): Form<RangeFormData>,
) -> Response {
    if let Some(response) = auth_redirect(&state, &jar).await {
        return response;
    }
    let snapshot = state.snapshot().await;
    let limit = match units::parse_limit(&form.limit) {
        Ok(limit) => limit,
        Err(error) => {
            return Html(render_range_editor_page(
                &snapshot,
                form.start_hour,
                form.end_hour,
                &form.limit,
                Some(&error.to_string()),
            ))
            .into_response();
        }
    };
    if form.start_hour >= form.end_hour || form.end_hour > 24 {
        return Html(render_range_editor_page(
            &snapshot,
            form.start_hour,
            form.end_hour,
            &form.limit,
            Some("range must satisfy 0 <= start < end <= 24"),
        ))
        .into_response();
    }
    let mut limits = snapshot.scheduler.schedule_limits_bps.to_vec();
    for entry in limits.iter_mut().take(form.end_hour).skip(form.start_hour) {
        *entry = limit;
    }
    let _ = state
        .execute(ApiRequest::SetSchedule { limits_bps: limits })
        .await;
    Redirect::to("/scheduler").into_response()
}

async fn delete_range(
    State(state): State<SharedDaemonState>,
    jar: CookieJar,
    Form(form): Form<RangeFormData>,
) -> Response {
    if let Some(response) = auth_redirect(&state, &jar).await {
        return response;
    }
    let snapshot = state.snapshot().await;
    let mut limits = snapshot.scheduler.schedule_limits_bps.to_vec();
    for entry in limits
        .iter_mut()
        .take(form.end_hour.min(24))
        .skip(form.start_hour)
    {
        *entry = None;
    }
    let _ = state
        .execute(ApiRequest::SetSchedule { limits_bps: limits })
        .await;
    Redirect::to("/scheduler").into_response()
}

async fn set_scheduler_mode(
    State(state): State<SharedDaemonState>,
    jar: CookieJar,
    Form(form): Form<ModeFormData>,
) -> Response {
    if let Some(response) = auth_redirect(&state, &jar).await {
        return response;
    }
    let mode = if form.mode == "scheduled" {
        ManualOrScheduled::Scheduled
    } else {
        ManualOrScheduled::Manual
    };
    let _ = state.execute(ApiRequest::SetMode { mode }).await;
    Redirect::to("/scheduler").into_response()
}

async fn set_quick_speed_mode(
    State(state): State<SharedDaemonState>,
    jar: CookieJar,
    Form(form): Form<QuickSpeedFormData>,
) -> Response {
    if let Some(response) = auth_redirect(&state, &jar).await {
        return response;
    }
    let result = match form.action.as_str() {
        "scheduled" => {
            state
                .execute(ApiRequest::SetMode {
                    mode: ManualOrScheduled::Scheduled,
                })
                .await
        }
        "manual" => {
            state
                .execute(ApiRequest::SetMode {
                    mode: ManualOrScheduled::Manual,
                })
                .await
        }
        "unlimited" => {
            if let Err(error) = state
                .execute(ApiRequest::SetManualLimit { limit_bps: None })
                .await
            {
                Err(error)
            } else {
                state
                    .execute(ApiRequest::SetMode {
                        mode: ManualOrScheduled::Manual,
                    })
                    .await
            }
        }
        "slow" => {
            if let Err(error) = state
                .execute(ApiRequest::SetManualLimit {
                    limit_bps: Some(256 * 1024),
                })
                .await
            {
                Err(error)
            } else {
                state
                    .execute(ApiRequest::SetMode {
                        mode: ManualOrScheduled::Manual,
                    })
                    .await
            }
        }
        _ => return (StatusCode::BAD_REQUEST, "unknown speed mode").into_response(),
    };
    if let Err(error) = result {
        return (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()).into_response();
    }
    Redirect::to(&normalize_next_path(form.return_to.as_deref())).into_response()
}

async fn routing_page(
    State(state): State<SharedDaemonState>,
    jar: CookieJar,
    Query(query): Query<ItemQuery>,
) -> Response {
    if let Some(response) = auth_redirect(&state, &jar).await {
        return response;
    }
    let snapshot = state.snapshot().await;
    Html(render_routing_page(&snapshot, query.test.as_deref(), None)).into_response()
}

async fn new_rule_page(State(state): State<SharedDaemonState>, jar: CookieJar) -> Response {
    if let Some(response) = auth_redirect(&state, &jar).await {
        return response;
    }
    let snapshot = state.snapshot().await;
    Html(render_rule_editor_page(
        &snapshot,
        None,
        "",
        &snapshot.routing.default_download_dir,
        None,
    ))
    .into_response()
}

async fn edit_rule_page(
    State(state): State<SharedDaemonState>,
    jar: CookieJar,
    Path(path): Path<RulePath>,
) -> Response {
    if let Some(response) = auth_redirect(&state, &jar).await {
        return response;
    }
    let snapshot = state.snapshot().await;
    let rule = snapshot.routing.rules.get(path.index);
    match rule {
        Some(rule) => Html(render_rule_editor_page(
            &snapshot,
            Some(path.index),
            &rule.pattern,
            &rule.directory,
            None,
        ))
        .into_response(),
        None => Redirect::to("/routing").into_response(),
    }
}

async fn save_rule(
    State(state): State<SharedDaemonState>,
    jar: CookieJar,
    Form(form): Form<RoutingRuleFormData>,
) -> Response {
    if let Some(response) = auth_redirect(&state, &jar).await {
        return response;
    }
    let snapshot = state.snapshot().await;
    let is_fallback = form.pattern.trim() == "*";
    let rule = DownloadRoutingRule {
        pattern: form.pattern.clone(),
        directory: form.directory.clone(),
    };
    if let Err(error) = validate_rule(&rule, is_fallback) {
        return Html(render_rule_editor_page(
            &snapshot,
            form.index,
            &form.pattern,
            &form.directory,
            Some(&error.to_string()),
        ))
        .into_response();
    }
    let mut rules = snapshot
        .routing
        .rules
        .iter()
        .filter(|rule| rule.pattern != "*")
        .cloned()
        .collect::<Vec<_>>();
    if is_fallback {
        let _ = state
            .execute(ApiRequest::SetDownloadRouting {
                default_download_dir: form.directory,
                rules,
            })
            .await;
    } else if let Some(index) = form.index {
        let nonfallback_index = snapshot.routing.rules[..index]
            .iter()
            .filter(|rule| rule.pattern != "*")
            .count();
        if nonfallback_index < rules.len() {
            rules[nonfallback_index] = rule;
        } else {
            rules.push(rule);
        }
        let _ = state
            .execute(ApiRequest::SetDownloadRouting {
                default_download_dir: snapshot.routing.default_download_dir.clone(),
                rules,
            })
            .await;
    } else {
        rules.push(rule);
        let _ = state
            .execute(ApiRequest::SetDownloadRouting {
                default_download_dir: snapshot.routing.default_download_dir.clone(),
                rules,
            })
            .await;
    }
    Redirect::to("/routing").into_response()
}

async fn delete_rule(
    State(state): State<SharedDaemonState>,
    jar: CookieJar,
    Path(path): Path<RulePath>,
) -> Response {
    if let Some(response) = auth_redirect(&state, &jar).await {
        return response;
    }
    let snapshot = state.snapshot().await;
    if snapshot
        .routing
        .rules
        .get(path.index)
        .is_some_and(|rule| rule.pattern == "*")
    {
        return Redirect::to("/routing").into_response();
    }
    let mut nonfallback_index = 0usize;
    let rules = snapshot
        .routing
        .rules
        .iter()
        .enumerate()
        .filter_map(|(idx, rule)| {
            if rule.pattern == "*" {
                None
            } else {
                let include = nonfallback_index != index_to_nonfallback(&snapshot, path.index, idx);
                nonfallback_index += 1;
                if include { Some(rule.clone()) } else { None }
            }
        })
        .collect::<Vec<_>>();
    let _ = state
        .execute(ApiRequest::SetDownloadRouting {
            default_download_dir: snapshot.routing.default_download_dir.clone(),
            rules,
        })
        .await;
    Redirect::to("/routing").into_response()
}

async fn move_rule_up(
    State(state): State<SharedDaemonState>,
    jar: CookieJar,
    Path(path): Path<RulePath>,
) -> Response {
    move_rule(state, jar, path.index, -1).await
}

async fn move_rule_down(
    State(state): State<SharedDaemonState>,
    jar: CookieJar,
    Path(path): Path<RulePath>,
) -> Response {
    move_rule(state, jar, path.index, 1).await
}

async fn move_rule(
    state: SharedDaemonState,
    jar: CookieJar,
    full_index: usize,
    delta: isize,
) -> Response {
    if let Some(response) = auth_redirect(&state, &jar).await {
        return response;
    }
    let snapshot = state.snapshot().await;
    if snapshot
        .routing
        .rules
        .get(full_index)
        .is_some_and(|rule| rule.pattern == "*")
    {
        return Redirect::to("/routing").into_response();
    }
    let mut rules = snapshot
        .routing
        .rules
        .iter()
        .filter(|rule| rule.pattern != "*")
        .cloned()
        .collect::<Vec<_>>();
    let index = snapshot.routing.rules[..full_index]
        .iter()
        .filter(|rule| rule.pattern != "*")
        .count();
    if index >= rules.len() {
        return Redirect::to("/routing").into_response();
    }
    let new_index =
        (index as isize + delta).clamp(0, rules.len().saturating_sub(1) as isize) as usize;
    rules.swap(index, new_index);
    let _ = state
        .execute(ApiRequest::SetDownloadRouting {
            default_download_dir: snapshot.routing.default_download_dir.clone(),
            rules,
        })
        .await;
    Redirect::to("/routing").into_response()
}

async fn reorder_rule(
    State(state): State<SharedDaemonState>,
    jar: CookieJar,
    Form(form): Form<ReorderFormData>,
) -> Response {
    if let Some(response) = auth_redirect(&state, &jar).await {
        return response;
    }
    let Some(source_text) = form.source.as_deref() else {
        return Redirect::to("/routing").into_response();
    };
    let Ok(source_full) = source_text.parse::<usize>() else {
        return Redirect::to("/routing").into_response();
    };
    let Ok(target_full) = form.target.parse::<usize>() else {
        return Redirect::to("/routing").into_response();
    };
    let snapshot = state.snapshot().await;
    if snapshot
        .routing
        .rules
        .get(source_full)
        .is_none_or(|rule| rule.pattern == "*")
        || snapshot
            .routing
            .rules
            .get(target_full)
            .is_none_or(|rule| rule.pattern == "*")
    {
        return Redirect::to("/routing").into_response();
    }
    let source = snapshot.routing.rules[..source_full]
        .iter()
        .filter(|rule| rule.pattern != "*")
        .count();
    let target = snapshot.routing.rules[..target_full]
        .iter()
        .filter(|rule| rule.pattern != "*")
        .count();
    let mut rules = snapshot
        .routing
        .rules
        .iter()
        .filter(|rule| rule.pattern != "*")
        .cloned()
        .collect::<Vec<_>>();
    if source < rules.len() && target < rules.len() && source != target {
        let rule = rules.remove(source);
        let insert_at = if form.position == "after" {
            target + usize::from(source > target)
        } else {
            target.saturating_sub(usize::from(source < target))
        }
        .min(rules.len());
        rules.insert(insert_at, rule);
        let _ = state
            .execute(ApiRequest::SetDownloadRouting {
                default_download_dir: snapshot.routing.default_download_dir.clone(),
                rules,
            })
            .await;
    }
    Redirect::to("/routing").into_response()
}

async fn webhooks_page(State(state): State<SharedDaemonState>, jar: CookieJar) -> Response {
    if let Some(response) = auth_redirect(&state, &jar).await {
        return response;
    }
    let snapshot = state.snapshot().await;
    Html(render_webhooks_page(&snapshot, None)).into_response()
}

async fn save_webhooks(
    State(state): State<SharedDaemonState>,
    jar: CookieJar,
    Form(form): Form<WebhookFormData>,
) -> Response {
    if let Some(response) = auth_redirect(&state, &jar).await {
        return response;
    }
    let snapshot = state.snapshot().await;
    let ping_mode = match form.ping_mode.as_str() {
        "everyone" => WebhookPingMode::Everyone,
        "specific_id" => WebhookPingMode::SpecificId,
        _ => WebhookPingMode::None,
    };
    if let Err(error) = validate_discord_webhook_url(&form.discord_webhook_url) {
        return Html(render_webhooks_page(&snapshot, Some(&error.to_string()))).into_response();
    }
    if let Err(error) = validate_ping_id(ping_mode, Some(&form.ping_id)) {
        return Html(render_webhooks_page(&snapshot, Some(&error.to_string()))).into_response();
    }
    let _ = state
        .execute(ApiRequest::SetWebhookSettings {
            discord_webhook_url: form.discord_webhook_url,
            ping_mode,
            ping_id: Some(form.ping_id),
        })
        .await;
    Redirect::to("/webhooks").into_response()
}

async fn trigger_webhook_test(State(state): State<SharedDaemonState>, jar: CookieJar) -> Response {
    if let Some(response) = auth_redirect(&state, &jar).await {
        return response;
    }
    let snapshot = state.snapshot().await;
    match state.execute(ApiRequest::TriggerWebhookTest).await {
        Ok(_) => Redirect::to("/webhooks").into_response(),
        Err(error) => {
            Html(render_webhooks_page(&snapshot, Some(&error.to_string()))).into_response()
        }
    }
}

async fn web_ui_page(State(state): State<SharedDaemonState>, jar: CookieJar) -> Response {
    if let Some(response) = auth_redirect(&state, &jar).await {
        return response;
    }
    let snapshot = state.snapshot().await;
    Html(render_web_ui_page(&snapshot, None)).into_response()
}

async fn save_web_ui(
    State(state): State<SharedDaemonState>,
    jar: CookieJar,
    Form(form): Form<WebUiFormData>,
) -> Response {
    if let Some(response) = auth_redirect(&state, &jar).await {
        return response;
    }
    let snapshot = state.snapshot().await;
    if let Err(error) = validate_bind_address(&form.bind_address) {
        return Html(render_web_ui_page(&snapshot, Some(&error.to_string()))).into_response();
    }
    if let Err(error) = validate_cookie_days(form.cookie_days) {
        return Html(render_web_ui_page(&snapshot, Some(&error.to_string()))).into_response();
    }
    let enabled = form.enabled.is_some();
    match state
        .execute(ApiRequest::SetWebUiSettings {
            enabled,
            bind_address: form.bind_address,
            port: form.port,
            cookie_days: form.cookie_days,
        })
        .await
    {
        Ok(reply) => {
            if !enabled {
                Html(render_disabled_message()).into_response()
            } else {
                Html(render_web_ui_page(&reply.snapshot, None)).into_response()
            }
        }
        Err(error) => Html(render_web_ui_page(&snapshot, Some(&error.to_string()))).into_response(),
    }
}

async fn authenticated(state: &SharedDaemonState, jar: &CookieJar) -> Result<bool> {
    let Some(cookie) = jar.get(AUTH_COOKIE_NAME) else {
        return Ok(false);
    };
    Ok(session_is_valid(state.as_ref(), cookie.value()).await)
}

async fn auth_redirect(state: &SharedDaemonState, jar: &CookieJar) -> Option<Response> {
    auth_redirect_with_next(state, jar, None).await
}

async fn auth_redirect_with_next(
    state: &SharedDaemonState,
    jar: &CookieJar,
    next: Option<&str>,
) -> Option<Response> {
    match authenticated(state, jar).await {
        Ok(true) => None,
        _ => Some(Redirect::to(&login_path(next)).into_response()),
    }
}

fn normalize_next_path(next: Option<&str>) -> String {
    let candidate = next.unwrap_or("/").trim();
    if candidate.starts_with('/') && !candidate.starts_with("//") {
        candidate.to_string()
    } else {
        "/".into()
    }
}

fn root_next_path(next: Option<&str>) -> String {
    let next = normalize_next_path(next);
    if next == "/" { "/current".into() } else { next }
}

fn login_success_path(next: Option<&str>) -> String {
    normalize_next_path(next)
}

fn login_path(next: Option<&str>) -> String {
    let next = normalize_next_path(next);
    if next == "/" {
        "/login".into()
    } else {
        let query = form_urlencoded::Serializer::new(String::new())
            .append_pair("next", &next)
            .finish();
        format!("/login?{query}")
    }
}

fn render_login(pin: &str, next: &str) -> String {
    let body = format!(
        r#"<section class="card narrow-card">
<div id="login-loading" class="login-loading">
<div class="spinner" aria-hidden="true"></div>
<p class="muted">Checking existing browser session...</p>
</div>
<div id="login-pairing" class="hidden">
<h2>Browser pairing</h2>
<p>Type this PIN into the terminal UI in the <strong>Web UI</strong> tab to approve this browser:</p>
<p class="pin">{}</p>
<p class="muted">The page will continue automatically after approval.</p>
<div id="pairing-status" class="muted">Waiting for terminal approval...</div>
</div>
</section>"#,
        esc(pin)
    );
    render_public_shell("Login", &body, Some(next))
}

fn render_public_shell(title: &str, body: &str, login_next: Option<&str>) -> String {
    format!(
        r##"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{}</title>
<style>{}</style>
</head>
<body>
<main class="wrap narrow">
<div class="public-brand"><span class="brand-mark" aria-hidden="true">A</span><span>AriaTUI</span></div>
{}
</main>
<div id="toast-region" class="toast-region" role="status" aria-live="polite"></div>
<script>{}</script>
</body>
</html>"##,
        esc(title),
        styles(),
        body,
        script(login_next)
    )
}

fn render_shell(
    snapshot: &Snapshot,
    active: WebTab,
    body: &str,
    auto_refresh: bool,
    page_title: &str,
) -> String {
    let mut tabs = String::new();
    for tab in WebTab::all() {
        let class = if tab == active { "tab active" } else { "tab" };
        let current = if tab == active {
            r#" aria-current="page""#
        } else {
            ""
        };
        let _ = write!(
            tabs,
            r#"<a class="{class}" href="{}"{current}>{}</a>"#,
            tab.href(),
            esc(tab.title())
        );
    }
    format!(
        r##"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{}</title>
<style>{}</style>
</head>
<body data-autorefresh="{}">
<a class="skip-link" href="#page-body">Skip to content</a>
<main class="app-shell">
<aside class="sidebar">
<a class="brand" href="/current" aria-label="AriaTUI downloads"><span class="brand-mark" aria-hidden="true">A</span><span>AriaTUI</span></a>
<nav class="tabs" aria-label="Main navigation">{}</nav>
<form method="post" action="/logout" class="logout"><button type="submit">Log out</button></form>
</aside>
<div class="workspace">
<header id="app-header" class="header">{}</header>
<section id="page-body" tabindex="-1">{}</section>
</div>
</main>
<div id="navigation-progress" class="navigation-progress" aria-hidden="true"></div>
<div id="toast-region" class="toast-region" role="status" aria-live="polite"></div>
<dialog id="app-dialog" class="app-dialog" aria-labelledby="dialog-title">
<div class="dialog-bar"><span id="dialog-title">Edit</span><button type="button" class="icon-button" data-dialog-close aria-label="Close">×</button></div>
<div id="dialog-content"></div>
</dialog>
<script>{}</script>
</body>
</html>"##,
        esc(page_title),
        styles(),
        if auto_refresh { "1" } else { "0" },
        tabs,
        render_header(snapshot, active.href()),
        body,
        script(None)
    )
}

fn render_header(snapshot: &Snapshot, return_to: &str) -> String {
    let lifecycle = format!("{:?}", snapshot.aria2_status.lifecycle).to_lowercase();
    let connected = lifecycle == "running";
    let mode_label = match snapshot.scheduler.mode {
        ManualOrScheduled::Scheduled => "Scheduled",
        ManualOrScheduled::Manual if snapshot.scheduler.manual_limit_bps.is_none() => "Unlimited",
        ManualOrScheduled::Manual => "Manual",
    };
    let effective_limit = snapshot
        .scheduler
        .effective_limit_bps
        .map(format_bytes_per_sec)
        .unwrap_or_else(|| "Unlimited".into());
    let manual_limit = snapshot
        .scheduler
        .manual_limit_bps
        .map(format_bytes_per_sec)
        .unwrap_or_else(|| "Unlimited".into());
    format!(
        r#"<div class="status-group" data-key="connection">
<span class="connection-status"><span class="status-dot {}"></span>aria2 {}</span>
<span class="sync-state" id="sync-state">Live</span>
</div>
<div class="headline-stats">
<span data-key="download-speed"><small>Down</small><strong>{}</strong></span>
<span data-key="upload-speed"><small>Up</small><strong>{}</strong></span>
<span data-key="active-count"><small>Active</small><strong>{}</strong></span>
<span data-key="queue-count"><small>Queued</small><strong>{}</strong></span>
</div>
<details class="speed-control" data-key="speed-control">
<summary><span><small>{}</small><strong>{}</strong></span><span aria-hidden="true">⌄</span></summary>
<div class="speed-menu">
<div class="speed-menu-heading"><strong>Download speed</strong><span>{} now</span></div>
<form method="post" action="/scheduler/quick">
<input type="hidden" name="action" value="scheduled"><input type="hidden" name="return_to" value="{}">
<button class="speed-option {}"><span><strong>Follow schedule</strong><small>Use the hourly plan</small></span><span>✓</span></button>
</form>
<form method="post" action="/scheduler/quick">
<input type="hidden" name="action" value="manual"><input type="hidden" name="return_to" value="{}">
<button class="speed-option {}"><span><strong>Manual · {}</strong><small>Keep one limit until changed</small></span><span>✓</span></button>
</form>
<form method="post" action="/scheduler/quick">
<input type="hidden" name="action" value="unlimited"><input type="hidden" name="return_to" value="{}">
<button class="speed-option {}"><span><strong>Unlimited for now</strong><small>Your schedule stays saved</small></span><span>✓</span></button>
</form>
<form method="post" action="/scheduler/quick">
<input type="hidden" name="action" value="slow"><input type="hidden" name="return_to" value="{}">
<button class="speed-option {}"><span><strong>Slow mode · 256 KiB/s</strong><small>Quickly free up bandwidth</small></span><span>✓</span></button>
</form>
<a class="speed-manage" href="/scheduler">Edit schedule and limits <span>→</span></a>
</div>
</details>"#,
        if connected { "online" } else { "offline" },
        esc(&lifecycle),
        esc(&format_bytes_per_sec(snapshot.global.download_speed_bps)),
        esc(&format_bytes_per_sec(snapshot.global.upload_speed_bps)),
        snapshot.global.num_active,
        snapshot.global.num_waiting,
        esc(mode_label),
        esc(&effective_limit),
        esc(&effective_limit),
        esc(return_to),
        if snapshot.scheduler.mode == ManualOrScheduled::Scheduled {
            "active"
        } else {
            ""
        },
        esc(return_to),
        if snapshot.scheduler.mode == ManualOrScheduled::Manual
            && snapshot.scheduler.manual_limit_bps.is_some()
            && snapshot.scheduler.manual_limit_bps != Some(256 * 1024)
        {
            "active"
        } else {
            ""
        },
        esc(&manual_limit),
        esc(return_to),
        if snapshot.scheduler.mode == ManualOrScheduled::Manual
            && snapshot.scheduler.manual_limit_bps.is_none()
        {
            "active"
        } else {
            ""
        },
        esc(return_to),
        if snapshot.scheduler.mode == ManualOrScheduled::Manual
            && snapshot.scheduler.manual_limit_bps == Some(256 * 1024)
        {
            "active"
        } else {
            ""
        },
    )
}

fn selected_attr(selected: bool) -> &'static str {
    if selected { "selected" } else { "" }
}

fn current_path(query: &ItemQuery) -> String {
    let parsed = CurrentListQuery::from_query(query);
    format!(
        "/current{}",
        current_query_suffix(
            parsed.selected.as_deref(),
            &parsed.search,
            parsed.filter,
            parsed.sort,
        )
    )
}

fn history_path(query: &ItemQuery) -> String {
    let parsed = HistoryListQuery::from_query(query);
    format!(
        "/history{}",
        history_query_suffix(
            parsed.selected.as_deref(),
            &parsed.search,
            parsed.filter,
            parsed.sort,
        )
    )
}

fn current_query_suffix(
    selected_gid: Option<&str>,
    search: &str,
    filter: CurrentFilter,
    sort: CurrentSort,
) -> String {
    query_suffix(&[
        selected_gid.map(|value| ("selected", value.to_string())),
        (!search.trim().is_empty()).then(|| ("search", search.trim().to_string())),
        Some(("filter", filter.label().to_string())),
        Some(("sort", sort.label().to_string())),
    ])
}

fn history_query_suffix(
    selected_gid: Option<&str>,
    search: &str,
    filter: HistoryFilter,
    sort: HistorySort,
) -> String {
    query_suffix(&[
        selected_gid.map(|value| ("selected", value.to_string())),
        (!search.trim().is_empty()).then(|| ("search", search.trim().to_string())),
        Some(("filter", filter.label().to_string())),
        Some(("sort", sort.label().to_string())),
    ])
}

fn query_suffix(entries: &[Option<(&str, String)>]) -> String {
    let parts = entries
        .iter()
        .flatten()
        .map(|(key, value)| format!("{key}={}", encode_query_value(value)))
        .collect::<Vec<_>>();
    if parts.is_empty() {
        String::new()
    } else {
        format!("?{}", parts.join("&"))
    }
}

fn encode_query_value(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            let _ = write!(encoded, "%{:02X}", byte);
        }
    }
    encoded
}

fn render_current_page(
    snapshot: &Snapshot,
    query: &CurrentListQuery,
    message: Option<&str>,
    error: Option<&str>,
    auto_refresh: bool,
) -> String {
    let visible = current_visible_items(
        &snapshot.current_downloads,
        &query.search,
        query.filter,
        query.sort,
    );
    let selected = query
        .selected
        .as_deref()
        .and_then(|gid| visible.iter().copied().find(|item| item.gid == gid))
        .or_else(|| visible.first().copied());
    let selected_gid = selected.map(|item| item.gid.as_str());
    let mut rows = String::new();
    for item in visible.iter().copied() {
        let list_eta = project_scheduled_eta(Local::now(), snapshot, item)
            .map(|projection| projection.eta_seconds)
            .or(item.eta_seconds);
        let selected_class = if Some(item.gid.as_str()) == selected_gid {
            "selected"
        } else {
            ""
        };
        let item_query =
            current_query_suffix(Some(&item.gid), &query.search, query.filter, query.sort);
        let actions = match item.status {
            DownloadStatus::Active => format!(
                r#"<form method="post" action="/current/{gid}/pause{query}"><button class="icon-button" aria-label="Pause download" title="Pause">Ⅱ</button></form><a class="icon-button danger" data-dialog data-dialog-return="/current{query}" aria-label="Cancel download" title="Cancel" href="/current/{gid}/cancel{query}">×</a>"#,
                gid = esc(&item.gid),
                query = esc(&item_query),
            ),
            DownloadStatus::Paused => format!(
                r#"<button type="button" class="drag-handle" data-drag-handle aria-label="Reorder {name}" title="Drag to reorder">⠿</button><form method="post" action="/current/{gid}/resume{query}"><button class="icon-button" aria-label="Resume download" title="Resume">▶</button></form><a class="icon-button danger" data-dialog data-dialog-return="/current{query}" aria-label="Cancel download" title="Cancel" href="/current/{gid}/cancel{query}">×</a>"#,
                gid = esc(&item.gid),
                query = esc(&item_query),
                name = esc(&item.name),
            ),
            DownloadStatus::Waiting => format!(
                r#"<button type="button" class="drag-handle" data-drag-handle aria-label="Reorder {name}" title="Drag to reorder">⠿</button><a class="icon-button danger" data-dialog data-dialog-return="/current{query}" aria-label="Cancel download" title="Cancel" href="/current/{gid}/cancel{query}">×</a>"#,
                gid = esc(&item.gid),
                query = esc(&item_query),
                name = esc(&item.name),
            ),
            _ => String::new(),
        };
        let reorder_attributes = if matches!(
            item.status,
            DownloadStatus::Paused | DownloadStatus::Waiting
        ) {
            format!(
                r#" draggable="true" data-reorder-kind="download" data-reorder-id="{}" data-reorder-url="/current/{}/reorder{}""#,
                esc(&item.gid),
                esc(&item.gid),
                esc(&item_query)
            )
        } else {
            String::new()
        };
        let progress = if item.total_bytes == 0 {
            0.0
        } else {
            (item.completed_bytes as f64 / item.total_bytes as f64 * 100.0).clamp(0.0, 100.0)
        };
        let _ = write!(
            rows,
            r#"<article class="download-item {selected_class}" data-key="download-{gid}" data-gid="{gid}" data-status="{status}"{reorder_attributes}>
<a class="download-main" href="/current{item_href}" aria-label="View details for {name}">
<div class="download-heading"><strong>{name}</strong><span class="status-badge {status}">{status}</span></div>
<div class="progress-track" role="progressbar" aria-valuemin="0" aria-valuemax="100" aria-valuenow="{progress:.0}"><span style="width:{progress:.2}%"></span></div>
<div class="download-meta"><span>{progress_text} · {done} of {total}</span><span>{speed}</span><span>{eta}</span></div>
</a>
<div class="item-actions">{actions}</div>
</article>"#,
            gid = esc(&item.gid),
            status = esc(status_label(&item.status)),
            name = esc(&item.name),
            item_href = esc(&current_query_suffix(
                Some(&item.gid),
                &query.search,
                query.filter,
                query.sort,
            )),
            progress = progress,
            progress_text = esc(&progress_text(item)),
            done = esc(&format_bytes(item.completed_bytes)),
            total = esc(&format_bytes(item.total_bytes)),
            speed = esc(&format_bytes_per_sec(item.download_speed_bps)),
            eta = esc(&format_eta(list_eta)),
            actions = actions,
            reorder_attributes = reorder_attributes,
        );
    }
    if rows.is_empty() {
        rows.push_str(
            r#"<div class="empty-state"><strong>No downloads here</strong><p>Try another filter, or add a download to get started.</p><a class="button primary" data-dialog data-dialog-return="/current" href="/current/add">Add download</a></div>"#,
        );
    }

    let mut body = String::new();
    if let Some(message) = message {
        let _ = write!(body, "<p class=\"message\">{}</p>", esc(message));
    }
    if let Some(error) = error {
        let _ = write!(body, "<p class=\"error\">{}</p>", esc(error));
    }
    let current_query = current_query_suffix(selected_gid, &query.search, query.filter, query.sort);
    let _ = write!(
        body,
        r#"<div class="page-heading">
<div><p class="eyebrow">Queue</p><h1>Downloads</h1><p class="muted">{visible_count} shown · {total_count} total</p></div>
<a class="button primary" data-dialog data-dialog-return="/current" href="/current/add"><span aria-hidden="true">＋</span> Add download</a>
</div>
<div class="toolbar">
<form method="get" action="/current" class="filter-bar" data-live-form>
<label class="search-field"><span class="sr-only">Search downloads</span><input type="search" name="search" value="{search}" placeholder="Search downloads…"></label>
<select name="filter">
<option value="all" {filter_all}>All</option>
<option value="active" {filter_active}>Active</option>
<option value="waiting" {filter_waiting}>Waiting</option>
<option value="paused" {filter_paused}>Paused</option>
</select>
<select name="sort">
<option value="queue" {sort_queue}>Queue</option>
<option value="name" {sort_name}>Name</option>
<option value="progress" {sort_progress}>Progress</option>
<option value="speed" {sort_speed}>Speed</option>
<option value="eta" {sort_eta}>ETA</option>
</select>
</form>
<div class="bulk-actions">
<form method="post" action="/current/pause-all{query}"><button class="button subtle">Pause all</button></form>
<form method="post" action="/current/resume-all{query}"><button class="button subtle">Resume all</button></form>
</div>
</div>
<p class="interaction-hint"><span class="drag-handle-symbol">⠿</span> Drag queued or paused downloads to change their order.</p>
"#,
        search = esc(&query.search),
        filter_all = selected_attr(query.filter == CurrentFilter::All),
        filter_active = selected_attr(query.filter == CurrentFilter::Active),
        filter_waiting = selected_attr(query.filter == CurrentFilter::Waiting),
        filter_paused = selected_attr(query.filter == CurrentFilter::Paused),
        sort_queue = selected_attr(query.sort == CurrentSort::Queue),
        sort_name = selected_attr(query.sort == CurrentSort::Name),
        sort_progress = selected_attr(query.sort == CurrentSort::Progress),
        sort_speed = selected_attr(query.sort == CurrentSort::Speed),
        sort_eta = selected_attr(query.sort == CurrentSort::Eta),
        query = esc(&current_query),
        visible_count = visible.len(),
        total_count = snapshot.current_downloads.len(),
    );
    body.push_str("<div class=\"split\">");
    let _ = write!(
        body,
        r#"<section class="card download-list" data-key="download-list" aria-label="Current downloads">
{}
</section>"#,
        rows
    );
    let _ = write!(
        body,
        r#"<aside class="card details-card" data-key="download-details"><p class="eyebrow">Selected</p><h2>Details</h2>{}</aside>"#,
        render_download_details(selected, snapshot)
    );
    body.push_str("</div>");
    render_shell(snapshot, WebTab::Current, &body, auto_refresh, "Current")
}

fn render_add_url_page(
    snapshot: &Snapshot,
    error: Option<&str>,
    chooser: Option<(&str, &str, &str, &str)>,
    initial_url: Option<&str>,
) -> String {
    let mut body = String::new();
    if let Some(error) = error {
        let _ = write!(body, "<p class=\"error\">{}</p>", esc(error));
    }
    let _ = write!(
        body,
        r#"<div class="page-heading">
<div><p class="eyebrow">New download</p><h1>Add a link</h1><p class="muted">Paste a web, file transfer, or magnet link.</p></div>
</div>
<section class="card narrow-card add-source-card">
<form method="post" action="/current/add/resolve" class="stack">
<label for="download-url">Download link</label>
<input id="download-url" type="text" inputmode="url" name="url" value="{}" placeholder="https://example.com/file.iso" autocomplete="off" autofocus required />
<p class="muted">HTTP, HTTPS, FTP, SFTP and magnet links are supported.</p>
<div class="actions"><button type="submit" class="primary">Continue</button><a class="button" href="/current">Cancel</a></div>
</form>
</section>"#,
        esc(initial_url.unwrap_or(""))
    );
    if let Some((url, url_filename, remote_label, remote_filename)) = chooser {
        let preview = match match_rule(
            &snapshot.routing.default_download_dir,
            &snapshot.routing.rules,
            remote_filename,
        ) {
            Ok(route) => route
                .resolved_directory
                .join(remote_filename)
                .display()
                .to_string(),
            Err(error) => error.to_string(),
        };
        let _ = write!(
            body,
            r#"<section class="card narrow-card filename-card">
<h2>Choose filename</h2>
<form method="post" action="/current/add/confirm" class="stack">
<input type="hidden" name="url" value="{}" />
<label><input type="radio" name="filename_choice" value="{}"> URL filename: {}</label>
<label><input type="radio" name="filename_choice" value="{}" checked> {}: {}</label>
<label><input type="radio" name="filename_choice" value="__custom__"> Use a custom filename</label>
<label>Custom filename</label>
<input type="text" name="custom_filename" value="{}" />
<p class="muted">Routing preview: {}</p>
<div class="actions"><button type="submit">Add download</button></div>
</form>
</section>"#,
            esc(url),
            esc(url_filename),
            esc(url_filename),
            esc(remote_filename),
            esc(remote_label),
            esc(remote_filename),
            esc(remote_filename),
            esc(&preview)
        );
    }
    render_shell(snapshot, WebTab::Current, &body, false, "Add URI")
}

fn extension_add_path(url: &str) -> String {
    let query = form_urlencoded::Serializer::new(String::new())
        .append_pair("url", url)
        .finish();
    format!("/extension/add?{query}")
}

fn render_extension_add_shell(title: &str, body: &str, close_on_load: bool) -> String {
    let close_script = if close_on_load {
        r#"<script>
setTimeout(() => {
  try { window.close(); } catch (_) {}
}, 500);
</script>"#
    } else {
        ""
    };
    format!(
        r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{}</title>
<style>{}</style>
</head>
<body>
<main class="wrap narrow">
<section class="card narrow-card">{}</section>
</main>
{}
</body>
</html>"#,
        esc(title),
        styles(),
        body,
        close_script
    )
}

fn render_extension_add_prompt(
    url: &str,
    url_filename: &str,
    remote_label: &str,
    remote_filename: &str,
    final_url: Option<&str>,
    error: Option<&str>,
) -> String {
    let mut body = String::new();
    body.push_str("<h2>Choose filename</h2>");
    if let Some(error) = error {
        let _ = write!(body, "<p class=\"error\">{}</p>", esc(error));
    }
    let _ = write!(
        body,
        r#"<p class="muted">Source: {}</p>
<form method="post" action="/extension/add" class="stack">
<input type="hidden" name="url" value="{}" />
<label><input type="radio" name="filename_choice" value="{}"> URL filename: {}</label>
<label><input type="radio" name="filename_choice" value="{}" checked> {}: {}</label>
<label><input type="radio" name="filename_choice" value="__custom__"> Use a custom filename</label>
<label>Custom filename</label>
<input type="text" name="custom_filename" value="{}" />
<div class="actions"><button type="submit">Add download</button></div>
</form>"#,
        esc(final_url.unwrap_or(url)),
        esc(url),
        esc(url_filename),
        esc(url_filename),
        esc(remote_filename),
        esc(remote_label),
        esc(remote_filename),
        esc(remote_filename),
    );
    render_extension_add_shell("Choose Filename", &body, false)
}

fn render_extension_add_prompt_from_submission(url: &str, error: &str) -> String {
    let body = format!(
        r#"<h2>Download not queued</h2>
<p class="error">{}</p>
<div class="actions"><a class="button" href="{}">Back</a></div>"#,
        esc(error),
        esc(&extension_add_path(url))
    );
    render_extension_add_shell("Download Not Queued", &body, false)
}

fn render_extension_add_done(display_name: &str, final_url: Option<&str>) -> String {
    let body = format!(
        r#"<h2>Queued</h2>
<p>{}</p>
<p class="muted">{}</p>"#,
        esc(display_name),
        esc(final_url.unwrap_or("This window will close automatically."))
    );
    render_extension_add_shell("Queued", &body, true)
}

fn render_extension_add_error(message: &str) -> String {
    let body = format!(
        r#"<h2>Download not queued</h2>
<p class="error">{}</p>"#,
        esc(message)
    );
    render_extension_add_shell("Download Not Queued", &body, false)
}

fn render_cancel_page(snapshot: &Snapshot, gid: &str, error: Option<&str>) -> String {
    let item = snapshot
        .current_downloads
        .iter()
        .find(|item| item.gid == gid);
    let mut body = String::new();
    if let Some(error) = error {
        let _ = write!(body, "<p class=\"error\">{}</p>", esc(error));
    }
    let _ = write!(
        body,
        r#"<div class="page-heading"><div><p class="eyebrow">Confirm action</p><h1>Cancel download?</h1></div></div>
<section class="card narrow-card">
<h2>{}</h2>
<p class="muted">The download will be removed from the queue. Choose whether to keep anything already downloaded.</p>
<form method="post" action="/current/{}/cancel" class="stack">
<label><input type="radio" name="delete_files" value="false" checked> Keep partial files</label>
<label><input type="radio" name="delete_files" value="true"> Delete partial files</label>
<label>Remember behavior</label>
<select name="remember_behavior">
<option value="">Do not change</option>
<option value="ask">Always ask</option>
<option value="keep_partials">Always keep partials</option>
<option value="delete_partials">Always delete partials</option>
</select>
<div class="actions"><button type="submit" class="danger">Cancel download</button><a class="button" href="/current">Keep downloading</a></div>
</form>
</section>"#,
        esc(&item
            .map(|item| item.name.clone())
            .unwrap_or_else(|| gid.to_string())),
        esc(gid)
    );
    render_shell(snapshot, WebTab::Current, &body, false, "Cancel Download")
}

fn render_history_page(snapshot: &Snapshot, query: &HistoryListQuery) -> String {
    let visible = history_visible_items(
        &snapshot.history_downloads,
        &query.search,
        query.filter,
        query.sort,
    );
    let selected = query
        .selected
        .as_deref()
        .and_then(|gid| visible.iter().copied().find(|item| item.gid == gid))
        .or_else(|| visible.first().copied());
    let selected_gid = selected.map(|item| item.gid.as_str());
    let mut rows = String::new();
    for item in visible.iter().copied() {
        let item_query =
            history_query_suffix(Some(&item.gid), &query.search, query.filter, query.sort);
        let selected_class = if Some(item.gid.as_str()) == selected_gid {
            "selected"
        } else {
            ""
        };
        let _ = write!(
            rows,
            r#"<article class="download-item activity-item {selected_class}" data-key="activity-{gid}" data-gid="{gid}">
<a class="download-main" href="/history{item_query}">
<div class="download-heading"><strong>{name}</strong><span class="status-badge {status}">{status}</span></div>
<div class="download-meta"><span>{size}</span><span>{error}</span></div>
</a>
<div class="item-actions"><form method="post" action="/history/{gid}/remove{item_query}"><button class="button subtle">Forget</button></form></div>
</article>"#,
            gid = esc(&item.gid),
            item_query = esc(&item_query),
            name = esc(&item.name),
            status = esc(status_label(&item.status)),
            size = esc(&format_bytes(item.total_bytes)),
            error = esc(item.error_message.as_deref().unwrap_or("No error")),
        );
    }
    if rows.is_empty() {
        rows.push_str(
            r#"<div class="empty-state"><strong>No matching activity</strong><p>Completed and removed downloads will appear here.</p></div>"#,
        );
    }
    let history_query = history_query_suffix(selected_gid, &query.search, query.filter, query.sort);
    let body = format!(
        r#"<div class="page-heading">
<div><p class="eyebrow">Past downloads</p><h1>Activity</h1><p class="muted">{visible_count} shown · {total_count} total</p></div>
</div>
<div class="toolbar">
<form method="get" action="/history" class="filter-bar" data-live-form>
<label class="search-field"><span class="sr-only">Search activity</span><input type="search" name="search" value="{search}" placeholder="Search activity…"></label>
<select name="filter">
<option value="all" {filter_all}>All</option>
<option value="complete" {filter_complete}>Complete</option>
<option value="error" {filter_error}>Error</option>
<option value="removed" {filter_removed}>Removed</option>
</select>
<select name="sort">
<option value="recent" {sort_recent}>Recent</option>
<option value="name" {sort_name}>Name</option>
<option value="size" {sort_size}>Size</option>
<option value="status" {sort_status}>Status</option>
</select>
</form>
<form method="post" action="/history/purge{history_query}"><button class="danger">Clear history</button></form>
</div>
<div class="split">
<section class="card download-list" data-key="activity-list" aria-label="Download activity">
{rows}
</section>
<aside class="card details-card" data-key="activity-details"><p class="eyebrow">Selected</p><h2>Details</h2>{details}</aside>
</div>"#,
        search = esc(&query.search),
        filter_all = selected_attr(query.filter == HistoryFilter::All),
        filter_complete = selected_attr(query.filter == HistoryFilter::Complete),
        filter_error = selected_attr(query.filter == HistoryFilter::Error),
        filter_removed = selected_attr(query.filter == HistoryFilter::Removed),
        sort_recent = selected_attr(query.sort == HistorySort::Recent),
        sort_name = selected_attr(query.sort == HistorySort::Name),
        sort_size = selected_attr(query.sort == HistorySort::Size),
        sort_status = selected_attr(query.sort == HistorySort::Status),
        history_query = esc(&history_query),
        visible_count = visible.len(),
        total_count = snapshot.history_downloads.len(),
        rows = rows,
        details = render_download_details(selected, snapshot)
    );
    render_shell(snapshot, WebTab::History, &body, true, "History")
}

fn render_scheduler_page(snapshot: &Snapshot, error: Option<&str>) -> String {
    let ranges = scheduler_ranges(snapshot);
    let current_hour = snapshot.scheduler.current_hour as usize;
    let active_range = ranges
        .iter()
        .find(|(start, end, _)| *start <= current_hour && current_hour < *end)
        .copied()
        .unwrap_or((current_hour, (current_hour + 1).min(24), None));
    let active_range_label = format!("{:02}:00–{:02}:00", active_range.0, active_range.1);
    let schedule_is_active = snapshot.scheduler.mode == ManualOrScheduled::Scheduled;
    let current_status = if schedule_is_active {
        format!(
            "{} is active now at {}",
            active_range_label,
            format_limit(active_range.2)
        )
    } else {
        format!(
            "Schedule paused · {} would be {}",
            active_range_label,
            format_limit(active_range.2)
        )
    };
    let mut body = String::new();
    if let Some(error) = error {
        let _ = write!(body, "<p class=\"error\">{}</p>", esc(error));
    }
    let _ = write!(
        body,
        r#"<div class="page-heading">
<div><p class="eyebrow">Bandwidth</p><h1>Speed schedule</h1><p class="muted">Shape the day by painting limits directly onto the timeline.</p></div>
</div>
<section class="schedule-status">
<div>
<span class="live-kicker"><span class="status-dot {}"></span>{}</span>
<strong>{}</strong>
<small>Next change {}</small>
</div>
<form method="post" action="/scheduler/mode" class="mode-switch" data-live-submit aria-label="Speed control mode">
<label><input type="radio" name="mode" value="scheduled" {}><span>Follow schedule</span></label>
<label><input type="radio" name="mode" value="manual" {}><span>Manual</span></label>
<button type="submit" class="sr-only">Apply mode</button>
</form>
</section>
<section class="card schedule-card">
<div class="schedule-card-heading">
<div><h2>Today’s plan</h2><p class="muted">Drag across hours to select them. Click once for a single hour.</p></div>
<div class="schedule-legend"><span><i class="now"></i>Now</span><span><i class="selected"></i>Selection</span></div>
</div>
{}
<div class="schedule-axis-note"><span>Midnight</span><span>Noon</span><span>Midnight</span></div>
</section>
<section class="schedule-secondary">
<div class="card">
<p class="eyebrow">Manual limit</p>
<p class="muted">Used whenever Manual mode is active.</p>
<form method="post" action="/scheduler/manual" class="inline-setting">
<label class="sr-only" for="manual-limit">Manual download limit</label>
<input id="manual-limit" name="value" value="{}" data-speed-input required>
<button type="submit">Save</button>
</form>
<small class="input-hint">Examples: 500K, 10M, unlimited</small>
</div>
<div class="card">
<p class="eyebrow">Connection baseline</p>
<p class="muted">Used to make scheduled ETA predictions realistic.</p>
<form method="post" action="/scheduler/usual" class="inline-setting">
<label class="sr-only" for="usual-speed">Usual connection speed</label>
<input id="usual-speed" name="value" value="{}" data-speed-input required>
<button type="submit">Save</button>
</form>
<small class="input-hint">Used only for ETA modelling</small>
</div>
</section>
"#,
        if schedule_is_active { "online" } else { "" },
        if schedule_is_active {
            "Schedule running"
        } else {
            "Schedule paused"
        },
        esc(&current_status),
        esc(&snapshot.scheduler.next_change_at_local),
        if schedule_is_active { "checked" } else { "" },
        if !schedule_is_active { "checked" } else { "" },
        render_interactive_schedule(snapshot, active_range),
        esc(&format_limit(snapshot.scheduler.manual_limit_bps)),
        esc(&format_limit(snapshot.scheduler.usual_internet_speed_bps)),
    );
    render_shell(snapshot, WebTab::Scheduler, &body, true, "Scheduler")
}

fn render_torrents_page(snapshot: &Snapshot, error: Option<&str>) -> String {
    let mut body = String::new();
    if let Some(error) = error {
        let _ = write!(body, "<p class=\"error\">{}</p>", esc(error));
    }
    let mode = snapshot.torrents.mode;
    let _ = write!(
        body,
        r#"<div class="page-heading">
<div><p class="eyebrow">New torrents</p><h1>Streaming priority</h1><p class="muted">Make media usable sooner while it downloads.</p></div>
</div>
<section class="card narrow-card">
<h2>Piece priority</h2>
<p class="muted">These defaults apply only to new magnet and remote .torrent downloads.</p>
<p class="muted">aria2 does not support true sequential torrent download. This feature uses <code>bt-prioritize-piece</code> to favor the beginning of files, and optionally the end as well.</p>
<form method="post" action="/torrents" class="stack">
<label>Mode</label>
<select name="mode">
<option value="off" {}>Off</option>
<option value="start_first" {}>Start first</option>
<option value="start_and_end_first" {}>Start and end first</option>
</select>
<label>Start-first size (MiB)</label>
<input type="number" name="head_size_mib" min="1" max="8192" value="{}">
<label>End-first size (MiB)</label>
<input type="number" name="tail_size_mib" min="1" max="8192" value="{}">
<p>Current aria2 option: <code>{}</code></p>
<p class="muted">Typical values: start first 32 MiB, end first 4 MiB. Start + end first is useful for media containers that store indexes near the end of the file.</p>
<div class="actions"><button type="submit" class="primary">Save settings</button></div>
</form>
</section>"#,
        if mode == TorrentStreamingMode::Off {
            "selected"
        } else {
            ""
        },
        if mode == TorrentStreamingMode::StartFirst {
            "selected"
        } else {
            ""
        },
        if mode == TorrentStreamingMode::StartAndEndFirst {
            "selected"
        } else {
            ""
        },
        snapshot.torrents.head_size_mib,
        snapshot.torrents.tail_size_mib,
        esc(snapshot
            .torrents
            .aria2_prioritize_piece
            .as_deref()
            .unwrap_or("off"),),
    );
    render_shell(snapshot, WebTab::Torrents, &body, true, "Torrent Streaming")
}

fn render_limit_editor_page(
    snapshot: &Snapshot,
    tab: WebTab,
    title: &str,
    action: &str,
    value: &str,
    hint: &str,
    error: Option<&str>,
) -> String {
    let mut body = String::new();
    if let Some(error) = error {
        let _ = write!(body, "<p class=\"error\">{}</p>", esc(error));
    }
    let _ = write!(
        body,
        r#"<section class="card narrow-card">
<h2>{}</h2>
<form method="post" action="{}" class="stack">
<input type="text" name="value" value="{}">
<p class="muted">{}</p>
<div class="actions"><button type="submit">Save</button><a class="button" href="{}">Back</a></div>
</form>
</section>"#,
        esc(title),
        esc(action),
        esc(value),
        esc(hint),
        esc(tab.href())
    );
    render_shell(snapshot, tab, &body, false, title)
}

fn render_range_editor_page(
    snapshot: &Snapshot,
    start: usize,
    end: usize,
    limit: &str,
    error: Option<&str>,
) -> String {
    let mut body = String::new();
    if let Some(error) = error {
        let _ = write!(body, "<p class=\"error\">{}</p>", esc(error));
    }
    let _ = write!(
        body,
        r#"<section class="card narrow-card">
<h2>Schedule range</h2>
<form method="post" action="/scheduler/range/save" class="stack">
<label>Start hour</label>
<input type="number" name="start_hour" min="0" max="23" value="{}">
<label>End hour</label>
<input type="number" name="end_hour" min="1" max="24" value="{}">
<label>Limit</label>
<input type="text" name="limit" value="{}">
<p class="muted">Examples: 10M, 10 mb/s, 1 kbps, unlimited.</p>
<div class="actions"><button type="submit">Save range</button><a class="button" href="/scheduler">Back</a></div>
</form>
</section>"#,
        start,
        end,
        esc(limit)
    );
    render_shell(snapshot, WebTab::Scheduler, &body, false, "Schedule Range")
}

fn render_routing_page(
    snapshot: &Snapshot,
    test_name: Option<&str>,
    error: Option<&str>,
) -> String {
    let mut rows = String::new();
    for (index, rule) in snapshot.routing.rules.iter().enumerate() {
        let kind = if rule.pattern == "*" {
            "fallback"
        } else {
            "regex"
        };
        let actions = if rule.pattern == "*" {
            format!(
                r#"<a class="button" data-dialog data-dialog-return="/routing" href="/routing/rule/{index}/edit">Edit</a>"#
            )
        } else {
            format!(
                r#"<button type="button" class="drag-handle" data-drag-handle aria-label="Reorder rule" title="Drag to reorder">⠿</button>
<a class="button" data-dialog data-dialog-return="/routing" href="/routing/rule/{index}/edit">Edit</a>
<form method="post" action="/routing/rule/{index}/delete"><button class="danger">Delete</button></form>"#
            )
        };
        let reorder_attributes = if rule.pattern == "*" {
            String::new()
        } else {
            format!(
                r#" draggable="true" data-reorder-kind="rule" data-reorder-id="{index}" data-reorder-url="/routing/rule/reorder""#
            )
        };
        let _ = write!(
            rows,
            r#"<tr data-key="rule-{index}"{reorder_attributes}><td>{}</td><td>{}</td><td>{}</td><td class="actions">{}</td></tr>"#,
            esc(kind),
            esc(&rule.pattern),
            esc(&rule.directory),
            actions,
        );
    }
    let test_result = test_name.map(|name| {
        match match_rule(
            &snapshot.routing.default_download_dir,
            &snapshot.routing.rules,
            name,
        ) {
            Ok(route) => format!(
                "Rule {} matched: {} -> {}",
                route.index + 1,
                route.rule.pattern,
                route.resolved_directory.display()
            ),
            Err(error) => error.to_string(),
        }
    });
    let mut body = String::new();
    if let Some(error) = error {
        let _ = write!(body, "<p class=\"error\">{}</p>", esc(error));
    }
    let _ = write!(
        body,
        r#"<div class="page-heading">
<div><p class="eyebrow">Automatic organisation</p><h1>Download folders</h1><p class="muted">Route files by name before they enter the queue.</p></div>
<a class="button primary" data-dialog data-dialog-return="/routing" href="/routing/rule/new">＋ Add rule</a>
</div>
<div class="grid2">
<section class="card">
<h2>Folder rules</h2>
<p>Fallback folder: {}</p>
<p class="interaction-hint"><span class="drag-handle-symbol">⠿</span> Drag rules into priority order. The first match wins.</p>
<table>
<thead><tr><th>Type</th><th>Pattern</th><th>Directory</th><th>Actions</th></tr></thead>
<tbody>{}</tbody>
</table>
</section>
<section class="card">
<h2>Try a filename</h2>
<form method="get" action="/routing" class="stack" data-live-form>
<input type="search" name="test" value="{}" placeholder="example-file.iso">
</form>
<p>{}</p>
</section>
</div>"#,
        esc(&snapshot.routing.default_download_dir),
        rows,
        esc(test_name.unwrap_or("")),
        esc(test_result.as_deref().unwrap_or(
            "Type a dummy file name to see which rule matches and where it would download."
        )),
    );
    render_shell(snapshot, WebTab::Routing, &body, true, "Routing")
}

fn render_rule_editor_page(
    snapshot: &Snapshot,
    index: Option<usize>,
    pattern: &str,
    directory: &str,
    error: Option<&str>,
) -> String {
    let mut body = String::new();
    if let Some(error) = error {
        let _ = write!(body, "<p class=\"error\">{}</p>", esc(error));
    }
    let dir_status = describe_directory_input(directory).unwrap_or_else(|error| error.to_string());
    let _ = write!(
        body,
        r#"<section class="card narrow-card">
<h2>Routing rule</h2>
<form method="post" action="/routing/rule/save" class="stack">
{}
<label>Pattern</label>
<input type="text" name="pattern" value="{}">
<label>Directory</label>
<input type="text" name="directory" value="{}">
<p class="muted">{}</p>
<div class="actions"><button type="submit">Save</button><a class="button" href="/routing">Back</a></div>
</form>
</section>"#,
        index
            .map(|value| format!(r#"<input type="hidden" name="index" value="{value}">"#))
            .unwrap_or_default(),
        esc(pattern),
        esc(directory),
        esc(&dir_status),
    );
    render_shell(snapshot, WebTab::Routing, &body, false, "Routing Rule")
}

fn render_webhooks_page(snapshot: &Snapshot, error: Option<&str>) -> String {
    let mut body = String::new();
    if let Some(error) = error {
        let _ = write!(body, "<p class=\"error\">{}</p>", esc(error));
    }
    let ping_mode = match snapshot.webhooks.ping_mode {
        WebhookPingMode::None => "none",
        WebhookPingMode::Everyone => "everyone",
        WebhookPingMode::SpecificId => "specific_id",
    };
    let _ = write!(
        body,
        r#"<div class="page-heading">
<div><p class="eyebrow">Discord</p><h1>Notifications</h1><p class="muted">Know when a download finishes or needs attention.</p></div>
</div>
<section class="card narrow-card">
<h2>Delivery settings</h2>
<form method="post" action="/webhooks" class="stack">
<label>Discord webhook URL</label>
<input type="text" name="discord_webhook_url" value="{}">
<label>Ping mode</label>
<select name="ping_mode">
<option value="none" {}>No ping</option>
<option value="everyone" {}>@everyone</option>
<option value="specific_id" {}>Specific user/role ID</option>
</select>
<label>Specific ID</label>
<input type="text" name="ping_id" value="{}">
<p class="muted">Events: completed, failed, removed, aria2 restart.</p>
<div class="actions"><button type="submit" class="primary">Save changes</button></div>
</form>
<form method="post" action="/webhooks/test"><button type="submit" class="button subtle">Send a test</button></form>
</section>"#,
        esc(&snapshot.webhooks.discord_webhook_url),
        if ping_mode == "none" { "selected" } else { "" },
        if ping_mode == "everyone" {
            "selected"
        } else {
            ""
        },
        if ping_mode == "specific_id" {
            "selected"
        } else {
            ""
        },
        esc(snapshot.webhooks.ping_id.as_deref().unwrap_or("")),
    );
    render_shell(snapshot, WebTab::Webhooks, &body, true, "Webhooks")
}

fn render_web_ui_page(snapshot: &Snapshot, error: Option<&str>) -> String {
    let mut body = String::new();
    if let Some(error) = error {
        let _ = write!(body, "<p class=\"error\">{}</p>", esc(error));
    }
    let _ = write!(
        body,
        r#"<div class="page-heading">
<div><p class="eyebrow">Browser access</p><h1>Access</h1><p class="muted">Control where this interface is available and who is paired.</p></div>
</div>
<section class="access-summary">
<div class="summary-card"><small>Status</small><strong>{:?}</strong></div>
<div class="summary-card"><small>Address</small><strong>{}</strong></div>
<div class="summary-card"><small>Sessions</small><strong>{}</strong></div>
</section>
<section class="card narrow-card">
<h2>Connection settings</h2>
<p class="muted">Pairing security: {} · Pending PINs: {}</p>
{}
<form method="post" action="/web-ui" class="stack">
<label><input type="checkbox" name="enabled" {}> Enabled</label>
<label>Bind address</label>
<input type="text" name="bind_address" value="{}">
<label>Port</label>
<input type="number" name="port" min="1" max="65535" value="{}">
<label>Cookie lifetime (days)</label>
<input type="number" name="cookie_days" min="1" max="365" value="{}">
<div class="actions"><button type="submit" class="primary">Save settings</button></div>
</form>
</section>"#,
        snapshot.web_ui.status,
        esc(&snapshot.web_ui.url),
        snapshot.web_ui.active_session_count,
        if snapshot.web_ui.auth_configured {
            "ready"
        } else {
            "not ready"
        },
        if snapshot.web_ui.pending_pair_pins.is_empty() {
            "-".to_string()
        } else {
            snapshot.web_ui.pending_pair_pins.join(", ")
        },
        snapshot
            .web_ui
            .last_error
            .as_ref()
            .map(|error| format!(r#"<p class="error">{}</p>"#, esc(error)))
            .unwrap_or_default(),
        if snapshot.web_ui.enabled {
            "checked"
        } else {
            ""
        },
        esc(&snapshot.web_ui.bind_address),
        snapshot.web_ui.port,
        snapshot.web_ui.cookie_days,
    );
    render_shell(snapshot, WebTab::WebUi, &body, true, "Web UI")
}

fn render_disabled_message() -> String {
    render_public_shell(
        "Web UI disabled",
        "<p>The web UI has been disabled. This page will stop working as soon as the daemon closes the listener.</p>",
        Some("/current"),
    )
}

fn render_download_details(item: Option<&DownloadItem>, snapshot: &Snapshot) -> String {
    let Some(item) = item else {
        return "<p>No item selected.</p>".into();
    };
    let now = Local::now();
    let projection = project_scheduled_eta(now, snapshot, item);
    let mut extra = String::new();
    if item.info_hash.is_some() || item.num_seeders.is_some() || item.belongs_to.is_some() {
        let _ = write!(
            extra,
            r#"
<dt>Torrent info hash</dt><dd>{}</dd>
<dt>Peers</dt><dd>{}</dd>
<dt>Seeders</dt><dd>{}</dd>"#,
            esc(item.info_hash.as_deref().unwrap_or("-")),
            esc(&item
                .connections
                .map(|value| value.to_string())
                .unwrap_or_else(|| "-".into())),
            esc(&item
                .num_seeders
                .map(|value| value.to_string())
                .unwrap_or_else(|| "-".into())),
        );
        if item.is_metadata_only {
            let _ = write!(
                extra,
                r#"<dt>Metadata follow-up GIDs</dt><dd>{}</dd>"#,
                esc(&item.followed_by.join(", "))
            );
        }
        if let Some(parent) = &item.belongs_to {
            let _ = write!(extra, r#"<dt>Parent GID</dt><dd>{}</dd>"#, esc(parent));
        }
    }
    let projected_eta = projection.as_ref().map(|projection| projection.eta_seconds);
    let projected_speed = projection
        .as_ref()
        .map(|projection| format_bytes_per_sec(projection.projected_now_speed_bps))
        .unwrap_or_else(|| "--".into());
    let projected_phase_count = projection
        .as_ref()
        .map(|projection| projection.phase_count.to_string())
        .unwrap_or_else(|| "--".into());
    let projection_visual = projection
        .as_ref()
        .map(|projection| render_projection_visuals(now, projection))
        .unwrap_or_default();
    format!(
        r#"<dl class="details">
<dt>Name</dt><dd>{}</dd>
<dt>GID</dt><dd>{}</dd>
<dt>Progress</dt><dd>{} / {}</dd>
<dt>Speed</dt><dd>{}</dd>
<dt>Realtime speed</dt><dd>{}</dd>
<dt>ETA</dt><dd>{}</dd>
<dt>Projected Scheduled ETA</dt><dd>{}</dd>
<dt>Projected speed now</dt><dd>{}</dd>
<dt>Projection phases</dt><dd>{}</dd>
<dt>Path</dt><dd>{}</dd>
<dt>Source</dt><dd>{}</dd>
<dt>Error</dt><dd>{}</dd>
{}
</dl>{}"#,
        esc(&item.name),
        esc(&item.gid),
        esc(&format_bytes(item.completed_bytes)),
        esc(&format_bytes(item.total_bytes)),
        esc(&format_bytes_per_sec(item.download_speed_bps)),
        esc(&format_bytes_per_sec(item.realtime_download_speed_bps)),
        esc(&format_eta(item.eta_seconds)),
        esc(&format_eta(projected_eta)),
        esc(&projected_speed),
        esc(&projected_phase_count),
        esc(item.primary_path.as_deref().unwrap_or("-")),
        esc(item.source_uri.as_deref().unwrap_or("-")),
        esc(item.error_message.as_deref().unwrap_or("-")),
        extra,
        projection_visual,
    )
}

fn render_projection_visuals(
    now: chrono::DateTime<Local>,
    projection: &ScheduledEtaProjection,
) -> String {
    if projection.phases.is_empty() {
        return String::new();
    }
    format!(
        r#"<div class="projection-shell">{timeline}<ul class="phase-list">{phases}</ul></div>"#,
        timeline = render_projection_timeline(now, projection),
        phases = render_projection_phase_list(now, projection),
    )
}

fn render_projection_timeline(
    now: chrono::DateTime<Local>,
    projection: &ScheduledEtaProjection,
) -> String {
    let total_duration = projection
        .phases
        .iter()
        .map(|phase| phase.duration_seconds.max(1))
        .sum::<u64>()
        .max(1);
    let view_width = 580.0;
    let mut x = 0.0;
    let mut body = String::new();
    body.push_str(
        r#"<svg class="projection-chart" viewBox="0 0 580 92" role="img" aria-label="Projected scheduled ETA phases">"#,
    );
    body.push_str(r##"<rect x="0" y="0" width="580" height="92" rx="10" fill="#101010"/>"##);
    for phase in &projection.phases {
        let width =
            ((phase.duration_seconds.max(1) as f64 / total_duration as f64) * view_width).max(2.0);
        let fill = match &phase.end {
            ProjectionPhaseEnd::HourBoundary => "#4f8cff",
            ProjectionPhaseEnd::PeerCompleted { .. } => "#25c2a0",
            ProjectionPhaseEnd::SelectedCompleted => "#f2c94c",
        };
        let tooltip = format!(
            "{} | {} | {} | {}",
            phase_range_label(now, phase),
            format_bytes_per_sec(phase.projected_item_speed_bps),
            peer_summary(phase),
            phase_event_summary(phase)
        );
        let _ = write!(
            body,
            r#"<g><rect x="{x:.1}" y="18" width="{width:.1}" height="32" rx="4" fill="{fill}"><title>{title}</title></rect>"#,
            x = x,
            width = width,
            fill = fill,
            title = esc(&tooltip),
        );
        if width >= 86.0 {
            let _ = write!(
                body,
                r##"<text x="{:.1}" y="38" text-anchor="middle" fill="#101010" font-size="11">{}</text>"##,
                x + width / 2.0,
                esc(&format_bytes_per_sec(phase.projected_item_speed_bps))
            );
        }
        body.push_str("</g>");
        x += width;
    }
    body.push_str(r##"<text x="16" y="72" fill="#bdbdbd" font-size="11">Blue: schedule change · Green: peer finished · Gold: selected download finished</text>"##);
    body.push_str("</svg>");
    body
}

fn render_projection_phase_list(
    now: chrono::DateTime<Local>,
    projection: &ScheduledEtaProjection,
) -> String {
    let mut body = String::new();
    for phase in &projection.phases {
        let _ = write!(
            body,
            "<li>{} &nbsp; {} &nbsp; {}</li>",
            esc(&phase_range_label(now, phase)),
            esc(&format_bytes_per_sec(phase.projected_item_speed_bps)),
            esc(&phase_summary(phase))
        );
    }
    body
}

fn phase_range_label(now: chrono::DateTime<Local>, phase: &ScheduledEtaPhase) -> String {
    let start = if phase.start_offset_seconds == 0 {
        "now".into()
    } else {
        phase_clock_label(now, phase.start_offset_seconds)
    };
    let end = match &phase.end {
        ProjectionPhaseEnd::SelectedCompleted => "done".into(),
        _ => phase_clock_label(now, phase.start_offset_seconds + phase.duration_seconds),
    };
    format!("{start}-{end}")
}

fn phase_clock_label(now: chrono::DateTime<Local>, offset_seconds: u64) -> String {
    let timestamp = now + Duration::seconds(offset_seconds as i64);
    if timestamp.date_naive() == now.date_naive() {
        timestamp.format("%H:%M").to_string()
    } else {
        timestamp.format("%a %H:%M").to_string()
    }
}

fn phase_summary(phase: &ScheduledEtaPhase) -> String {
    let sharing = format!(
        "of {} aggregate, {}",
        format_bytes_per_sec(phase.projected_aggregate_speed_bps),
        peer_summary(phase)
    );
    match &phase.end {
        ProjectionPhaseEnd::HourBoundary => format!("{sharing} until schedule change"),
        ProjectionPhaseEnd::PeerCompleted { name } => format!("{sharing} until {name} finished"),
        ProjectionPhaseEnd::SelectedCompleted => sharing,
    }
}

fn phase_event_summary(phase: &ScheduledEtaPhase) -> &'static str {
    match &phase.end {
        ProjectionPhaseEnd::HourBoundary => "schedule change",
        ProjectionPhaseEnd::PeerCompleted { .. } => "peer finished",
        ProjectionPhaseEnd::SelectedCompleted => "selected download finished",
    }
}

fn peer_summary(phase: &ScheduledEtaPhase) -> String {
    if phase.peer_count == 0 {
        "full observed share".into()
    } else {
        format!("shared with {}", peer_names_summary(phase))
    }
}

fn peer_names_summary(phase: &ScheduledEtaPhase) -> String {
    let shown = phase.peer_names.iter().take(2).cloned().collect::<Vec<_>>();
    let mut summary = shown.join(", ");
    let remaining = phase.peer_count.saturating_sub(shown.len());
    if remaining > 0 {
        if !summary.is_empty() {
            summary.push_str(", ");
        }
        summary.push_str(&format!("+{remaining} more"));
    }
    summary
}

fn progress_text(item: &DownloadItem) -> String {
    if item.total_bytes == 0 {
        "0%".into()
    } else {
        Percentage(item.completed_bytes as f64 / item.total_bytes as f64).to_string()
    }
}

fn status_label(status: &DownloadStatus) -> &'static str {
    match status {
        DownloadStatus::Active => "active",
        DownloadStatus::Waiting => "waiting",
        DownloadStatus::Paused => "paused",
        DownloadStatus::Complete => "complete",
        DownloadStatus::Error => "error",
        DownloadStatus::Removed => "removed",
        DownloadStatus::Unknown => "unknown",
    }
}

fn scheduler_ranges(snapshot: &Snapshot) -> Vec<(usize, usize, Option<u64>)> {
    let limits = &snapshot.scheduler.schedule_limits_bps;
    if limits.is_empty() {
        return Vec::new();
    }
    let mut ranges = Vec::new();
    let mut start = 0usize;
    let mut current = limits[0];
    for (hour, &limit) in limits.iter().enumerate().skip(1) {
        if limit != current {
            ranges.push((start, hour, current));
            start = hour;
            current = limit;
        }
    }
    ranges.push((start, limits.len(), current));
    ranges
}

fn render_interactive_schedule(
    snapshot: &Snapshot,
    active_range: (usize, usize, Option<u64>),
) -> String {
    let limits = &snapshot.scheduler.schedule_limits_bps;
    let max_finite = limits.iter().flatten().copied().max().unwrap_or(1);
    let current_hour = snapshot.scheduler.current_hour as usize;
    let mut body = String::new();
    let _ = write!(
        body,
        r#"<div id="schedule-graph" class="schedule-graph" role="group" aria-label="24 hour speed schedule" data-max-bps="{max_finite}">
<div class="schedule-grid-lines" aria-hidden="true"><i></i><i></i><i></i><i></i></div>"#
    );
    for (hour, limit) in limits.iter().enumerate() {
        let level = match limit {
            None => 100.0,
            Some(value) => 18.0 + (*value as f64 / max_finite as f64 * 72.0),
        };
        let current = hour == current_hour;
        let in_active_segment = active_range.0 <= hour && hour < active_range.1;
        let formatted = format_limit(*limit);
        let _ = write!(
            body,
            r#"<button type="button" class="schedule-hour {} {}" data-key="schedule-hour-{hour}" data-schedule-hour="{}" data-limit="{}" data-original-level="{:.2}" aria-label="{:02}:00 to {:02}:00, {}">
<span class="hour-bar-space"><span class="hour-bar" style="--bar-height:{:.2}%"><i></i></span></span>
<span class="hour-label">{:02}</span>
<span class="hour-value">{}</span>
</button>"#,
            if current { "current" } else { "" },
            if in_active_segment {
                "active-segment"
            } else {
                ""
            },
            hour,
            esc(&formatted),
            level,
            hour,
            hour + 1,
            esc(&formatted),
            level,
            hour,
            esc(&formatted),
        );
    }
    body.push_str(
        r#"</div>
<div id="schedule-editor" class="schedule-editor" popover="manual">
<form method="post" action="/scheduler/range/save" class="stack">
<input type="hidden" name="start_hour" value="0">
<input type="hidden" name="end_hour" value="1">
<div class="schedule-editor-heading">
<div><p class="eyebrow">Selected hours</p><h3 id="schedule-selection-label">00:00–01:00</h3></div>
<button type="button" class="icon-button" data-close-schedule aria-label="Close">×</button>
</div>
<label for="schedule-limit">Target download speed</label>
<input id="schedule-limit" name="limit" type="text" inputmode="decimal" placeholder="10M, 500 kb/s, unlimited" autocomplete="off" required>
<div class="speed-presets" aria-label="Speed presets">
<button type="button" data-speed-preset="256K">256K</button>
<button type="button" data-speed-preset="1M">1M</button>
<button type="button" data-speed-preset="10M">10M</button>
<button type="button" data-speed-preset="unlimited">Unlimited</button>
</div>
<p id="schedule-limit-preview" class="input-preview">Type a speed using K, M, MB/s, or “unlimited”.</p>
<div class="actions"><button type="submit" class="primary">Apply to selection</button><span class="muted enter-hint">Enter to apply</span></div>
</form>
</div>"#,
    );
    body
}

fn prompt_candidate(resolved: &crate::daemon::ResolvedHttpUrl) -> Option<(&'static str, String)> {
    resolved
        .remote_filename
        .clone()
        .map(|filename| ("server filename", filename))
        .or_else(|| {
            resolved
                .redirect_filename
                .clone()
                .map(|filename| ("redirect target", filename))
        })
}

fn index_to_nonfallback(
    snapshot: &Snapshot,
    full_index: usize,
    candidate_full_index: usize,
) -> usize {
    snapshot.routing.rules[..candidate_full_index.min(full_index + 1)]
        .iter()
        .filter(|rule| rule.pattern != "*")
        .count()
        .saturating_sub(1)
}

fn esc(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn styles() -> &'static str {
    r#"
:root {
  color-scheme: dark;
  --bg: #0d120f;
  --surface: #151c18;
  --surface-soft: #1b241f;
  --ink: #edf4ef;
  --muted: #96a39b;
  --line: #2a352e;
  --brand: #35bd87;
  --brand-dark: #78dfb7;
  --brand-soft: #173b2e;
  --danger: #ff8d8d;
  --danger-soft: #3b2022;
  --warning: #e9b665;
  --shadow: 0 1px 2px rgba(0,0,0,.22), 0 12px 34px rgba(0,0,0,.2);
  font-family: Inter, ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
}
* { box-sizing: border-box; }
html { background: var(--bg); }
body { background: var(--bg); color: var(--ink); margin: 0; min-height: 100vh; font-size: 15px; line-height: 1.5; }
button, input, select { font: inherit; }
a { color: var(--brand-dark); }
h1, h2, h3, p { margin-top: 0; }
h1 { font-size: clamp(1.8rem, 3vw, 2.35rem); line-height: 1.1; letter-spacing: -.035em; margin-bottom: .4rem; }
h2 { font-size: 1.1rem; letter-spacing: -.015em; margin-bottom: 1rem; }
.wrap { max-width: 1160px; margin: 0 auto; padding: 2rem; }
.narrow { max-width: 540px; padding-top: 10vh; }
.narrow-card { max-width: 720px; }
.public-brand, .brand { display: flex; align-items: center; gap: .7rem; font-weight: 750; font-size: 1.05rem; color: var(--ink); text-decoration: none; }
.public-brand { margin-bottom: 1.25rem; }
.brand-mark { display: grid; place-items: center; width: 34px; height: 34px; border-radius: 11px; color: #07150f; background: var(--brand); font-size: .95rem; box-shadow: 0 6px 16px rgba(53,189,135,.18); }
.app-shell { display: grid; grid-template-columns: 220px minmax(0, 1fr); min-height: 100vh; }
.sidebar { position: sticky; top: 0; height: 100vh; padding: 1.6rem 1rem; background: #111713; border-right: 1px solid var(--line); display: flex; flex-direction: column; z-index: 10; }
.sidebar .brand { padding: 0 .65rem 1.5rem; }
.tabs { display: grid; gap: .25rem; }
.tab { min-width: 0; color: var(--muted); text-decoration: none; padding: .68rem .75rem; border-radius: 10px; font-weight: 600; transition: background .15s, color .15s, transform .15s; }
.tab:hover { background: var(--surface-soft); color: var(--ink); }
.tab:active { transform: scale(.98); }
.tab.active { background: var(--brand-soft); color: var(--brand-dark); }
.logout { margin-top: auto; }
.logout button { width: 100%; color: var(--muted); background: transparent; border-color: transparent; justify-content: flex-start; }
.workspace { min-width: 0; overflow: hidden; }
.header { min-height: 74px; padding: 1rem clamp(1rem, 3vw, 2.5rem); border-bottom: 1px solid var(--line); background: #0f1511; display: flex; align-items: center; justify-content: space-between; gap: 1rem; position: sticky; top: 0; z-index: 8; }
.status-group, .headline-stats { display: flex; align-items: center; gap: 1rem; }
.connection-status { font-weight: 650; display: flex; align-items: center; gap: .5rem; }
.status-dot { width: 8px; height: 8px; border-radius: 50%; background: #929b95; }
.status-dot.online { background: #20a36d; box-shadow: 0 0 0 4px rgba(32,163,109,.12); }
.status-dot.offline { background: #d35a5a; box-shadow: 0 0 0 4px rgba(211,90,90,.12); }
.sync-state { color: var(--muted); font-size: .82rem; }
.sync-state.syncing::before { content: ""; display: inline-block; width: 8px; height: 8px; margin-right: .4rem; border: 1.5px solid var(--line); border-top-color: var(--brand); border-radius: 50%; animation: spin .7s linear infinite; }
.headline-stats span { display: grid; min-width: 68px; }
.headline-stats small { color: var(--muted); font-size: .7rem; text-transform: uppercase; letter-spacing: .06em; }
.headline-stats strong { font-size: .92rem; }
.speed-control { position: relative; }
.speed-control > summary { list-style: none; min-width: 156px; display: flex; align-items: center; justify-content: space-between; gap: .75rem; padding: .48rem .7rem; border: 1px solid var(--line); border-radius: 10px; cursor: pointer; user-select: none; }
.speed-control > summary::-webkit-details-marker { display: none; }
.speed-control > summary:hover, .speed-control[open] > summary { background: var(--surface-soft); border-color: #45554a; }
.speed-control > summary span:first-child { display: grid; }
.speed-control > summary small { color: var(--muted); font-size: .66rem; text-transform: uppercase; letter-spacing: .06em; }
.speed-control > summary strong { font-size: .84rem; }
.speed-menu { position: absolute; z-index: 30; top: calc(100% + .55rem); right: 0; width: 310px; padding: .55rem; background: #121914; border: 1px solid var(--line); border-radius: 14px; box-shadow: 0 18px 50px rgba(0,0,0,.42); }
.speed-menu-heading { display: flex; align-items: center; justify-content: space-between; padding: .45rem .55rem .65rem; }
.speed-menu-heading span { color: var(--muted); font-size: .78rem; }
.speed-option { width: 100%; min-height: 0; justify-content: space-between; border-color: transparent; background: transparent; padding: .65rem .6rem; text-align: left; }
.speed-option > span:first-child { display: grid; }
.speed-option small { color: var(--muted); font-weight: 500; }
.speed-option > span:last-child { color: transparent; }
.speed-option.active { color: var(--brand-dark); background: var(--brand-soft); }
.speed-option.active > span:last-child { color: var(--brand); }
.speed-manage { display: flex; justify-content: space-between; margin-top: .35rem; padding: .7rem .6rem .4rem; border-top: 1px solid var(--line); text-decoration: none; font-weight: 650; }
#page-body { width: min(1280px, 100%); margin: 0 auto; padding: clamp(1.25rem, 3vw, 2.5rem); outline: none; }
.page-heading { display: flex; flex-wrap: wrap; align-items: flex-end; justify-content: space-between; gap: 1rem; margin-bottom: 1.5rem; }
.page-heading p { margin-bottom: 0; }
.eyebrow { color: var(--brand); font-size: .73rem; line-height: 1; text-transform: uppercase; letter-spacing: .11em; font-weight: 750; margin-bottom: .65rem; }
.card { background: var(--surface); border: 1px solid var(--line); border-radius: 16px; padding: clamp(1rem, 2vw, 1.35rem); box-shadow: var(--shadow); margin-bottom: 1rem; }
.split { display: grid; grid-template-columns: minmax(0, 1.8fr) minmax(280px, .85fr); gap: 1rem; align-items: start; }
.grid2 { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 1rem; align-items: start; }
.access-summary { display: grid; grid-template-columns: repeat(3, minmax(0, 1fr)); gap: .75rem; max-width: 720px; margin-bottom: 1rem; }
.summary-card { display: grid; gap: .25rem; min-width: 0; padding: 1rem; background: var(--surface); border: 1px solid var(--line); border-radius: 13px; }
.summary-card small { color: var(--muted); font-size: .72rem; text-transform: uppercase; letter-spacing: .06em; }
.summary-card strong { overflow: hidden; text-overflow: ellipsis; white-space: nowrap; }
.schedule-status { display: flex; align-items: center; justify-content: space-between; gap: 1rem; padding: 1rem 1.1rem; margin-bottom: 1rem; background: var(--surface); border: 1px solid var(--line); border-radius: 14px; }
.schedule-status > div:first-child { display: grid; gap: .2rem; }
.schedule-status > div:first-child > strong { font-size: 1.05rem; }
.schedule-status small { color: var(--muted); }
.live-kicker { display: flex; align-items: center; gap: .45rem; color: var(--brand-dark); font-size: .72rem; font-weight: 750; text-transform: uppercase; letter-spacing: .07em; }
.mode-switch { display: inline-grid; grid-template-columns: repeat(2, 1fr); padding: 3px; border: 1px solid var(--line); border-radius: 11px; background: #0f1511; }
.mode-switch label { cursor: pointer; }
.mode-switch input { position: absolute; opacity: 0; pointer-events: none; }
.mode-switch span { display: block; padding: .48rem .75rem; border-radius: 8px; color: var(--muted); white-space: nowrap; }
.mode-switch input:checked + span { color: var(--ink); background: var(--surface-soft); box-shadow: 0 1px 3px rgba(0,0,0,.22); }
.schedule-card { overflow-x: auto; padding: 1.15rem 1.2rem .9rem; }
.schedule-card-heading { min-width: 820px; display: flex; justify-content: space-between; gap: 1rem; }
.schedule-card-heading p { margin-bottom: .25rem; }
.schedule-legend { display: flex; align-items: flex-start; gap: 1rem; color: var(--muted); font-size: .78rem; }
.schedule-legend span { display: flex; align-items: center; gap: .35rem; }
.schedule-legend i { display: inline-block; width: 9px; height: 9px; border-radius: 3px; }
.schedule-legend i.now { background: var(--warning); }
.schedule-legend i.selected { background: var(--brand); }
.schedule-graph { isolation: isolate; position: relative; min-width: 820px; height: 310px; display: grid; grid-template-columns: repeat(24, minmax(0, 1fr)); gap: 3px; padding: 12px 0 0; user-select: none; touch-action: pan-x pan-y pinch-zoom; }
.schedule-grid-lines { position: absolute; z-index: -1; inset: 12px 0 58px; display: flex; flex-direction: column; justify-content: space-between; pointer-events: none; }
.schedule-grid-lines i { display: block; border-top: 1px dashed #2b3830; }
.schedule-hour { position: relative; min-width: 0; height: 100%; display: grid; grid-template-rows: 232px 24px 20px; gap: 2px; padding: 0 1px; color: var(--muted); border: 0; border-radius: 7px; background: transparent; }
.schedule-hour:hover { border: 0; background: #1c2721; transform: none; }
.schedule-hour.active-segment { background: rgba(53,189,135,.055); }
.schedule-hour.current { background: rgba(233,182,101,.08); }
.schedule-hour.current::after { content: ""; position: absolute; z-index: 3; inset: 0 auto 48px 50%; width: 1px; background: var(--warning); pointer-events: none; }
.schedule-hour.current .hour-label { color: var(--warning); font-weight: 800; }
.schedule-hour.is-selected { background: rgba(53,189,135,.14); }
.schedule-hour.is-selected .hour-label { color: var(--brand-dark); }
.hour-bar-space { height: 232px; display: flex; align-items: flex-end; padding: 0 3px; }
.hour-bar { position: relative; width: 100%; height: var(--bar-height); min-height: 5px; border-radius: 5px 5px 2px 2px; background: #3e6554; transition: height .12s ease, background .12s; }
.active-segment .hour-bar { background: #2d8e69; }
.current .hour-bar { background: #b88746; }
.is-selected .hour-bar { background: var(--brand); }
.hour-bar i { position: absolute; left: 50%; top: 0; width: 7px; height: 7px; border: 2px solid #111713; border-radius: 50%; background: currentColor; transform: translate(-50%, -45%); }
.hour-label { align-self: center; font-size: .67rem; font-variant-numeric: tabular-nums; }
.hour-value { position: absolute; z-index: 5; left: 50%; bottom: 47px; width: max-content; max-width: 120px; padding: .28rem .4rem; color: var(--ink); background: #27332c; border: 1px solid #435248; border-radius: 6px; box-shadow: 0 5px 15px rgba(0,0,0,.28); font-size: .65rem; opacity: 0; pointer-events: none; transform: translateX(-50%) translateY(4px); transition: opacity .12s, transform .12s; }
.schedule-hour:hover .hour-value, .schedule-hour:focus-visible .hour-value { opacity: 1; transform: translateX(-50%) translateY(0); }
.schedule-axis-note { min-width: 820px; display: flex; justify-content: space-between; padding-top: .3rem; color: var(--muted); border-top: 1px solid var(--line); font-size: .7rem; }
.schedule-secondary { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 1rem; max-width: 900px; }
.setting-value { display: block; margin: -.25rem 0 .35rem; font-size: 1.3rem; }
.schedule-editor { position: fixed; inset: auto; width: min(350px, calc(100vw - 2rem)); margin: 0; padding: 1rem; color: var(--ink); background: #151d18; border: 1px solid #405047; border-radius: 15px; box-shadow: 0 22px 70px rgba(0,0,0,.52); }
.schedule-editor::backdrop { background: transparent; }
.schedule-editor-heading { display: flex; align-items: flex-start; justify-content: space-between; gap: 1rem; }
.schedule-editor-heading h3 { margin: 0; font-size: 1.15rem; }
.speed-presets { display: grid; grid-template-columns: repeat(4, 1fr); gap: .35rem; }
.speed-presets button { min-width: 0; padding-inline: .3rem; font-size: .75rem; }
.input-preview { min-height: 24px; margin: 0; color: var(--muted); font-size: .82rem; }
.input-preview.valid { color: var(--brand-dark); }
.input-preview.invalid { color: var(--danger); }
.enter-hint { font-size: .74rem; }
.stack { display: flex; flex-direction: column; gap: .75rem; }
.inline, .actions, .item-actions, .bulk-actions { display: flex; gap: .45rem; align-items: center; flex-wrap: wrap; }
.inline-setting { display: grid; grid-template-columns: minmax(0, 1fr) auto; gap: .5rem; }
.input-hint { display: block; margin-top: .4rem; color: var(--muted); }
.interaction-hint { display: flex; align-items: center; gap: .4rem; margin: -.3rem 0 .85rem; color: var(--muted); font-size: .78rem; }
.drag-handle-symbol { font-size: 1rem; }
.toolbar { display: flex; align-items: center; justify-content: space-between; gap: .75rem; margin-bottom: 1rem; }
.filter-bar { display: grid; grid-template-columns: minmax(220px, 1fr) 130px 130px; gap: .55rem; width: min(680px, 100%); }
.filter-bar > * { min-width: 0; }
.search-field { position: relative; }
.table-wrap { overflow-x: auto; border: 1px solid var(--line); border-radius: 12px; }
table { width: 100%; border-collapse: collapse; }
th { color: var(--muted); background: var(--surface-soft); font-size: .72rem; text-transform: uppercase; letter-spacing: .06em; }
th, td { border-bottom: 1px solid var(--line); padding: .78rem; text-align: left; vertical-align: middle; }
tbody tr:last-child td { border-bottom: 0; }
tbody tr { transition: background .15s; }
tbody tr:hover { background: var(--surface-soft); }
label { color: #ced8d1; font-weight: 600; font-size: .9rem; }
input, select { width: 100%; min-height: 42px; color: var(--ink); background: #111713; border: 1px solid #39473e; border-radius: 10px; padding: .6rem .75rem; outline: none; transition: border .15s, box-shadow .15s, background .15s; }
input[type="radio"], input[type="checkbox"] { width: auto; min-height: auto; accent-color: var(--brand); }
input:focus, select:focus { border-color: var(--brand); box-shadow: 0 0 0 3px rgba(53,189,135,.15); }
input[aria-disabled="true"] { color: var(--muted); background: var(--surface-soft); }
form.is-dirty:not([data-live-form]) button[type="submit"] { box-shadow: 0 0 0 3px rgba(53,189,135,.15); }
.button, button { min-height: 38px; display: inline-flex; align-items: center; justify-content: center; gap: .35rem; color: #dce5df; background: var(--surface); border: 1px solid #3a483f; border-radius: 10px; padding: .48rem .78rem; text-decoration: none; font-weight: 650; cursor: pointer; transition: transform .12s, background .12s, border .12s, opacity .12s; }
.button:hover, button:hover { background: var(--surface-soft); border-color: #56685c; }
.button:active, button:active { transform: scale(.97); }
.button.primary, button.primary { color: #07150f; background: var(--brand); border-color: var(--brand); box-shadow: 0 5px 14px rgba(53,189,135,.16); }
.button.primary:hover, button.primary:hover { background: #57d39f; }
.button.subtle { color: var(--muted); background: transparent; }
.icon-button { width: 34px; min-height: 34px; padding: 0; border-radius: 9px; }
.drag-handle { width: 34px; min-height: 34px; padding: 0; color: var(--muted); border-color: transparent; background: transparent; cursor: grab; font-size: 1.15rem; touch-action: none; }
.drag-handle:active { cursor: grabbing; }
button[disabled], .is-busy { opacity: .58; cursor: wait; }
.message, .error { padding: .75rem 1rem; border-radius: 10px; }
.message { color: var(--brand-dark); background: var(--brand-soft); }
.error { color: #ffb1b1; background: var(--danger-soft); }
.muted { color: var(--muted); }
.danger { color: var(--danger) !important; border-color: #754044 !important; }
.download-list { padding: .35rem; overflow: hidden; }
.empty-state { padding: 3rem 1rem; text-align: center; color: var(--muted); }
.empty-state strong { display: block; color: var(--ink); font-size: 1.05rem; }
.empty-state p { margin: .35rem 0 1rem; }
.download-item { position: relative; display: grid; grid-template-columns: minmax(0, 1fr) auto; align-items: center; gap: .75rem; padding: .9rem; border-radius: 12px; transition: background .15s, opacity .15s; }
.download-item[draggable="true"] { cursor: default; }
.is-dragging { opacity: .38 !important; }
.drop-before::before, tr.drop-before td { box-shadow: inset 0 2px 0 var(--brand); }
.drop-after::after, tr.drop-after td { box-shadow: inset 0 -2px 0 var(--brand); }
.download-item.drop-before::before { inset: 0 .5rem auto !important; width: auto !important; height: 2px; }
.download-item.drop-after::after { content: ""; position: absolute; inset: auto .5rem 0; height: 2px; background: var(--brand); }
.download-item + .download-item { border-top: 1px solid var(--line); border-top-left-radius: 0; border-top-right-radius: 0; }
.download-item:hover, .download-item.selected { background: var(--surface-soft); }
.download-item.selected::before { content: ""; position: absolute; inset: .65rem auto .65rem 0; width: 3px; border-radius: 2px; background: var(--brand); }
.download-main { min-width: 0; color: inherit; text-decoration: none; }
.download-heading { display: flex; align-items: center; justify-content: space-between; gap: .75rem; margin-bottom: .6rem; }
.download-heading strong { white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.status-badge { border-radius: 999px; padding: .18rem .5rem; color: var(--muted); background: #222c26; font-size: .7rem; font-weight: 700; text-transform: capitalize; }
.status-badge.active, .status-badge.complete { color: var(--brand-dark); background: var(--brand-soft); }
.status-badge.error, .status-badge.removed { color: var(--danger); background: var(--danger-soft); }
.status-badge.waiting { color: var(--warning); background: #392d1c; }
.progress-track { height: 6px; overflow: hidden; border-radius: 999px; background: #29342d; }
.progress-track span { display: block; height: 100%; border-radius: inherit; background: var(--brand); transition: width .35s ease; }
.download-meta { display: flex; gap: 1rem; color: var(--muted); font-size: .78rem; margin-top: .55rem; }
.download-meta span:first-child { margin-right: auto; }
.details-card { position: sticky; top: 90px; max-height: calc(100vh - 112px); overflow: auto; }
.details { margin: 0; }
.details dt { color: var(--muted); font-size: .72rem; text-transform: uppercase; letter-spacing: .055em; margin-top: .8rem; }
.details dd { margin: .15rem 0 0; word-break: break-word; }
.chart-shell { overflow-x: auto; }
.schedule-chart { width: 100%; min-width: 584px; height: auto; display: block; }
.projection-shell { margin-top: .9rem; }
.projection-chart { width: 100%; height: auto; display: block; margin-bottom: .6rem; }
.phase-list { margin: 0; padding-left: 1.25rem; color: #c3cec7; }
.phase-list li { margin-bottom: .3rem; }
code { background: #0b100d; border: 1px solid var(--line); border-radius: 5px; padding: .15rem .3rem; }
.pin { font-variant-numeric: tabular-nums; font-size: 2.7rem; font-weight: 800; letter-spacing: .3rem; text-align: center; margin: 1.2rem 0; color: var(--brand); }
.hidden { display: none !important; }
.login-loading { display: grid; justify-items: center; gap: .8rem; padding: 1.2rem 0; }
.spinner { width: 1.6rem; height: 1.6rem; border: 2px solid var(--line); border-top-color: var(--brand); border-radius: 999px; animation: spin .8s linear infinite; }
.navigation-progress { position: fixed; z-index: 100; inset: 0 auto auto 0; width: 0; height: 3px; opacity: 0; background: var(--brand); transition: width .25s ease, opacity .2s; }
.navigation-progress.active { width: 72%; opacity: 1; }
.navigation-progress.done { width: 100%; opacity: 0; }
.app-dialog { width: min(680px, calc(100vw - 2rem)); max-height: min(820px, calc(100vh - 2rem)); margin: auto; padding: 0; color: var(--ink); background: #111713; border: 1px solid #3b4a40; border-radius: 17px; box-shadow: 0 28px 90px rgba(0,0,0,.58); overflow: hidden; }
.app-dialog::backdrop { background: rgba(4,8,6,.76); }
.dialog-bar { position: sticky; z-index: 2; top: 0; display: flex; align-items: center; justify-content: space-between; min-height: 54px; padding: .65rem .8rem .65rem 1rem; background: #151d18; border-bottom: 1px solid var(--line); font-weight: 750; }
#dialog-content { max-height: calc(100vh - 76px); padding: 1rem; overflow: auto; }
#dialog-content .page-heading { margin: 0 0 1rem; }
#dialog-content .dialog-page-heading { display: none; }
#dialog-content .card { margin: 0; box-shadow: none; }
#dialog-content .narrow-card { max-width: none; }
#dialog-content > .error { margin-bottom: .75rem; }
#dialog-content:has(.filename-card) .add-source-card { display: none; }
.toast-region { position: fixed; z-index: 110; right: 1.2rem; bottom: 1.2rem; display: grid; gap: .5rem; pointer-events: none; }
.toast { max-width: 360px; color: var(--ink); background: #243129; border: 1px solid #39483e; border-radius: 11px; padding: .72rem .9rem; box-shadow: 0 12px 32px rgba(0,0,0,.3); animation: toast-in .2s ease both; }
.toast.error { color: #ffe7e7; background: #662f34; }
.skip-link { position: fixed; z-index: 200; top: .5rem; left: .5rem; transform: translateY(-160%); background: var(--ink); color: #0a0f0c; padding: .5rem .75rem; border-radius: 8px; }
.skip-link:focus { transform: none; }
.sr-only { position: absolute; width: 1px; height: 1px; padding: 0; margin: -1px; overflow: hidden; clip: rect(0,0,0,0); white-space: nowrap; border: 0; }
@keyframes spin { to { transform: rotate(360deg); } }
@keyframes toast-in { from { opacity: 0; transform: translateY(8px); } }
@media (prefers-reduced-motion: reduce) { *, *::before, *::after { scroll-behavior: auto !important; animation-duration: .01ms !important; transition-duration: .01ms !important; } }
@media (max-width: 920px) {
  .app-shell { grid-template-columns: 1fr; padding-bottom: 66px; }
  .sidebar { position: fixed; inset: auto 0 0; width: 100%; height: 66px; padding: .45rem; border: 0; border-top: 1px solid var(--line); display: block; }
  .sidebar .brand, .logout { display: none; }
  .tabs { display: grid; grid-template-columns: repeat(7, minmax(0, 1fr)); gap: .1rem; }
  .tab { padding: .55rem .2rem; text-align: center; font-size: .68rem; overflow: hidden; text-overflow: ellipsis; }
  .header { position: static; min-height: 58px; padding: .75rem 1rem; }
  .headline-stats span:nth-child(n+3) { display: none; }
  .split, .grid2 { grid-template-columns: 1fr; }
  .schedule-secondary { grid-template-columns: 1fr; }
  .access-summary { grid-template-columns: 1fr; }
  .details-card { position: static; max-height: none; }
}
@media (max-width: 640px) {
  #page-body { padding: 1rem; }
  .headline-stats { display: none; }
  .connection-status { font-size: .8rem; }
  .sync-state { display: none; }
  .speed-control > summary { min-width: 132px; }
  .speed-menu { position: fixed; top: 64px; right: .65rem; width: min(310px, calc(100vw - 1.3rem)); }
  .page-heading { align-items: center; }
  .schedule-status { align-items: stretch; flex-direction: column; }
  .mode-switch { width: 100%; }
  .mode-switch span { text-align: center; }
  .toolbar { align-items: stretch; flex-direction: column; }
  .filter-bar { grid-template-columns: minmax(0, 1fr) 110px; }
  .filter-bar .search-field { grid-column: 1 / -1; }
  .filter-bar select { min-width: 0; }
  .bulk-actions { justify-content: flex-end; }
  .download-item { grid-template-columns: minmax(0, 1fr); }
  .item-actions { justify-content: flex-end; }
  .download-meta span:first-child { display: none; }
  .wrap { padding: 1rem; }
}
"#
}

fn script(login_next: Option<&str>) -> String {
    let script = r##"
const loginNext = __LOGIN_NEXT__;
const pairingStatus = document.getElementById("pairing-status");
const loginLoading = document.getElementById("login-loading");
const loginPairing = document.getElementById("login-pairing");
const progressBar = document.getElementById("navigation-progress");
const toastRegion = document.getElementById("toast-region");
let navigationController;
let refreshController;
let liveFormTimer;
let refreshInFlight = false;
let interactionInFlight = false;
let requestEpoch = 0;
let refreshPausedUntil = 0;
let scheduleDrag;
let draggedItem;
let navigationProgressEpoch = 0;

function pauseBackgroundRefresh(duration = 700) {
  refreshPausedUntil = Math.max(refreshPausedUntil, Date.now() + duration);
}

function setSyncState(label, syncing = false) {
  const node = document.getElementById("sync-state");
  if (!node) return;
  node.textContent = label;
  node.classList.toggle("syncing", syncing);
}

function setProgress(active) {
  if (!progressBar) return;
  progressBar.classList.toggle("active", active);
  if (!active) {
    progressBar.classList.add("done");
    setTimeout(() => progressBar.classList.remove("done"), 260);
  }
}

function toast(message, kind = "") {
  if (!toastRegion || !message) return;
  const node = document.createElement("div");
  node.className = `toast ${kind}`.trim();
  node.textContent = message;
  toastRegion.appendChild(node);
  setTimeout(() => node.remove(), 3200);
}

function focusSnapshot() {
  const active = document.activeElement;
  if (!active || !active.matches("input, select, textarea")) return null;
  return {
    id: active.id,
    name: active.name,
    start: active.selectionStart,
    end: active.selectionEnd
  };
}

function restoreFocus(snapshot) {
  if (!snapshot) return;
  const escaped = window.CSS && CSS.escape ? CSS.escape(snapshot.name || "") : snapshot.name;
  const node = snapshot.id
    ? document.getElementById(snapshot.id)
    : document.querySelector(`[name="${escaped}"]`);
  if (!node) return;
  node.focus({ preventScroll: true });
  if (typeof node.setSelectionRange === "function" && snapshot.start !== null) {
    node.setSelectionRange(snapshot.start, snapshot.end);
  }
}

function compatibleNodes(current, next) {
  return current?.nodeType === next.nodeType
    && (current.nodeType !== Node.ELEMENT_NODE || current.tagName === next.tagName);
}

function nodeKey(node) {
  if (node?.nodeType !== Node.ELEMENT_NODE) return null;
  const key = node.getAttribute("data-key") || node.id;
  return key ? `${node.tagName}:${key}` : null;
}

function shouldPreserveControl(element, options) {
  return options.preserveInteraction
    && element.matches?.("input, textarea, select")
    && (element === document.activeElement || element.form?.classList.contains("is-dirty"));
}

function morphAttributes(current, next, options, stats) {
  const preserveControl = shouldPreserveControl(current, options);
  const preserveOpen = current.tagName === "DETAILS" && current.open;
  for (const attribute of [...current.attributes]) {
    if ((preserveControl && ["value", "checked", "selected"].includes(attribute.name))
      || (preserveOpen && attribute.name === "open")) continue;
    if (!next.hasAttribute(attribute.name)) {
      current.removeAttribute(attribute.name);
      stats.attributes += 1;
    }
  }
  for (const attribute of [...next.attributes]) {
    if ((preserveControl && ["value", "checked", "selected"].includes(attribute.name))
      || (preserveOpen && attribute.name === "open")) continue;
    if (current.getAttribute(attribute.name) !== attribute.value) {
      current.setAttribute(attribute.name, attribute.value);
      stats.attributes += 1;
    }
  }
  if (!preserveControl) {
    if (current instanceof HTMLInputElement || current instanceof HTMLTextAreaElement) {
      if (current.value !== next.value) current.value = next.value;
      if (current instanceof HTMLInputElement) current.checked = next.checked;
    } else if (current instanceof HTMLSelectElement && current.value !== next.value) {
      current.value = next.value;
    }
  }
  if (preserveOpen) current.open = true;
}

function morphNode(current, next, options, stats) {
  if (!compatibleNodes(current, next)) {
    const replacement = next.cloneNode(true);
    current.replaceWith(replacement);
    stats.replaced += 1;
    return replacement;
  }
  if (current.nodeType === Node.TEXT_NODE || current.nodeType === Node.COMMENT_NODE) {
    if (current.data !== next.data) {
      current.data = next.data;
      stats.text += 1;
    }
    return current;
  }
  morphAttributes(current, next, options, stats);
  if (shouldPreserveControl(current, options)) return current;
  if (current.matches("[data-preserve-contents]")) return current;

  const oldChildren = [...current.childNodes];
  const keyed = new Map(
    oldChildren
      .map((child) => [nodeKey(child), child])
      .filter(([key]) => key)
  );
  const used = new Set();
  const desired = [];
  const nextChildren = [...next.childNodes];
  for (let index = 0; index < nextChildren.length; index += 1) {
    const nextChild = nextChildren[index];
    const key = nodeKey(nextChild);
    let match = key ? keyed.get(key) : null;
    if (!match) {
      const samePosition = oldChildren[index];
      if (samePosition && !used.has(samePosition) && !nodeKey(samePosition)
        && compatibleNodes(samePosition, nextChild)) {
        match = samePosition;
      }
    }
    if (!match) {
      match = nextChild.cloneNode(true);
      stats.added += 1;
    } else {
      morphNode(match, nextChild, options, stats);
      used.add(match);
    }
    desired.push(match);
  }

  let cursor = current.firstChild;
  for (const child of desired) {
    if (child === cursor) {
      cursor = cursor.nextSibling;
    } else {
      current.insertBefore(child, cursor);
    }
  }
  for (const child of [...current.childNodes]) {
    if (!desired.includes(child)) {
      child.remove();
      stats.removed += 1;
    }
  }
  return current;
}

function reconcileElement(current, next, preserveInteraction = true) {
  const stats = { added: 0, removed: 0, replaced: 0, text: 0, attributes: 0 };
  morphNode(current, next, { preserveInteraction }, stats);
  return stats;
}

function applyDocument(nextDocument, url, historyMode = "push", preserveFocus = false) {
  const nextBody = nextDocument.getElementById("page-body");
  const nextHeader = nextDocument.getElementById("app-header");
  if (!nextBody || !nextHeader) {
    window.location.href = url;
    return false;
  }
  const focus = preserveFocus ? focusSnapshot() : null;
  const currentBody = document.getElementById("page-body");
  const currentHeader = document.getElementById("app-header");
  const nextPath = new URL(url, window.location.href).pathname;
  const sameView = nextPath === window.location.pathname;
  let bodyPatch;
  if (sameView) {
    bodyPatch = reconcileElement(currentBody, nextBody, preserveFocus);
  } else {
    currentBody.replaceWith(nextBody);
    bodyPatch = { fullNavigation: true };
  }
  const headerPatch = reconcileElement(currentHeader, nextHeader, true);
  const nextTabs = nextDocument.querySelector(".tabs");
  const tabs = document.querySelector(".tabs");
  const tabsPatch = nextTabs && tabs ? reconcileElement(tabs, nextTabs, true) : {};
  window.__ariatuiLastPatch = { body: bodyPatch, header: headerPatch, tabs: tabsPatch };
  document.title = nextDocument.title;
  document.body.dataset.autorefresh = nextDocument.body.dataset.autorefresh || "0";
  if (historyMode === "push") history.pushState({}, "", url);
  if (historyMode === "replace") history.replaceState({}, "", url);
  restoreFocus(focus);
  syncConditionalFields();
  return true;
}

async function fetchDocument(url, options = {}) {
  const headers = { "X-Requested-With": "AriaTUI-Reactive", ...(options.headers || {}) };
  const response = await fetch(url, {
    ...options,
    credentials: "same-origin",
    headers
  });
  const text = await response.text();
  return {
    response,
    document: new DOMParser().parseFromString(text, "text/html")
  };
}

async function navigate(url, historyMode = "push", preserveFocus = false) {
  const epoch = ++requestEpoch;
  if (navigationProgressEpoch) navigationProgressEpoch = epoch;
  if (navigationController) navigationController.abort();
  if (refreshController) refreshController.abort();
  navigationController = new AbortController();
  interactionInFlight = true;
  const progressTimer = setTimeout(() => {
    if (epoch === requestEpoch) {
      navigationProgressEpoch = epoch;
      setProgress(true);
    }
  }, 220);
  try {
    const result = await fetchDocument(url, { signal: navigationController.signal });
    if (epoch !== requestEpoch) return;
    const finalUrl = result.response.url || url;
    if (applyDocument(result.document, finalUrl, historyMode, preserveFocus)) {
      setSyncState("Live");
    }
  } catch (error) {
    if (error.name !== "AbortError" && epoch === requestEpoch) {
      setSyncState("Offline");
      toast("Couldn’t reach AriaTUI. Your current view is still here.", "error");
    }
  } finally {
    clearTimeout(progressTimer);
    if (epoch === requestEpoch) {
      interactionInFlight = false;
    }
    if (navigationProgressEpoch === epoch) {
      navigationProgressEpoch = 0;
      setProgress(false);
    }
  }
}

function formUrl(form) {
  const url = new URL(form.action || window.location.href, window.location.href);
  url.search = new URLSearchParams(new FormData(form)).toString();
  return url.toString();
}

function syncConditionalFields() {
  const pingMode = document.querySelector('[name="ping_mode"]');
  const pingId = document.querySelector('[name="ping_id"]');
  if (pingMode && pingId) {
    const enabled = pingMode.value === "specific_id";
    pingId.readOnly = !enabled;
    pingId.setAttribute("aria-disabled", String(!enabled));
    pingId.closest("label")?.classList.toggle("muted", !enabled);
  }
  const torrentMode = document.querySelector('[name="mode"][value="off"]:checked, select[name="mode"]');
  if (torrentMode && torrentMode.closest('form[action="/torrents"]')) {
    const off = torrentMode.value === "off";
    for (const name of ["head_size_mib", "tail_size_mib"]) {
      const field = document.querySelector(`[name="${name}"]`);
      if (field) {
        field.readOnly = off;
        field.setAttribute("aria-disabled", String(off));
      }
    }
  }
}

function parseSpeedInput(input) {
  const value = input.trim();
  if (value.toLowerCase() === "unlimited") {
    return { bps: null, label: "Unlimited — no speed cap" };
  }
  const match = value.match(/^(\d+(?:\.\d+)?)\s*([a-zA-Z/ ._-]*)$/);
  if (!match) return null;
  const amount = Number(match[1]);
  let suffix = match[2].toUpperCase().replace(/[ ._-]/g, "").replace(/\/S$/, "").replace(/PS$/, "");
  const multipliers = {
    "": 1, B: 1, BYTE: 1, BYTES: 1,
    K: 1024, KB: 1024, KIB: 1024, KBYTE: 1024, KBYTES: 1024,
    M: 1048576, MB: 1048576, MIB: 1048576,
    G: 1073741824, GB: 1073741824, GIB: 1073741824
  };
  if (!Number.isFinite(amount) || amount < 0 || !(suffix in multipliers)) return null;
  const bps = Math.round(amount * multipliers[suffix]);
  const units = ["B/s", "KiB/s", "MiB/s", "GiB/s"];
  let shown = bps;
  let unit = 0;
  while (shown >= 1024 && unit < units.length - 1) {
    shown /= 1024;
    unit += 1;
  }
  return { bps, label: `${shown >= 10 || unit === 0 ? shown.toFixed(0) : shown.toFixed(1)} ${units[unit]}` };
}

function selectedScheduleHours() {
  return [...document.querySelectorAll("[data-schedule-hour].is-selected")];
}

function updateSchedulePreview() {
  const input = document.getElementById("schedule-limit");
  const preview = document.getElementById("schedule-limit-preview");
  const graph = document.getElementById("schedule-graph");
  if (!input || !preview || !graph) return;
  const parsed = parseSpeedInput(input.value);
  input.setCustomValidity(!parsed && input.value.trim() ? "Use a speed such as 500K, 10M, 25 MB/s, or unlimited." : "");
  preview.classList.toggle("valid", Boolean(parsed));
  preview.classList.toggle("invalid", !parsed && input.value.trim().length > 0);
  preview.textContent = parsed
    ? `${parsed.label} for ${selectedScheduleHours().length} selected hour${selectedScheduleHours().length === 1 ? "" : "s"}`
    : input.value.trim()
      ? "Try 500K, 10M, 25 MB/s, or unlimited."
      : "Type a speed using K, M, MB/s, or “unlimited”.";
  for (const hour of selectedScheduleHours()) {
    const bar = hour.querySelector(".hour-bar");
    if (!bar) continue;
    if (!parsed) {
      bar.style.setProperty("--bar-height", `${hour.dataset.originalLevel}%`);
      continue;
    }
    const max = Math.max(Number(graph.dataset.maxBps) || 1, parsed.bps || 0);
    const level = parsed.bps === null ? 100 : 18 + (parsed.bps / max * 72);
    bar.style.setProperty("--bar-height", `${level}%`);
  }
}

function setScheduleSelection(start, end) {
  for (const hour of document.querySelectorAll("[data-schedule-hour]")) {
    const value = Number(hour.dataset.scheduleHour);
    const selected = start <= value && value <= end;
    hour.classList.toggle("is-selected", selected);
    hour.setAttribute("aria-pressed", String(selected));
    if (!selected) {
      hour.querySelector(".hour-bar")?.style.setProperty("--bar-height", `${hour.dataset.originalLevel}%`);
    }
  }
}

function clearScheduleSelection() {
  for (const hour of selectedScheduleHours()) {
    hour.classList.remove("is-selected");
    hour.setAttribute("aria-pressed", "false");
    hour.querySelector(".hour-bar")?.style.setProperty("--bar-height", `${hour.dataset.originalLevel}%`);
  }
}

function closeScheduleEditor() {
  const editor = document.getElementById("schedule-editor");
  if (!editor) return;
  if (typeof editor.hidePopover === "function" && editor.matches(":popover-open")) {
    editor.hidePopover();
  } else {
    editor.classList.remove("open");
  }
  clearScheduleSelection();
}

function closeAppDialog() {
  const dialog = document.getElementById("app-dialog");
  if (dialog?.open) dialog.close();
  const content = document.getElementById("dialog-content");
  if (content) content.replaceChildren();
}

function updateAppDialog(nextDocument) {
  const nextBody = nextDocument.getElementById("page-body");
  const content = document.getElementById("dialog-content");
  const title = document.getElementById("dialog-title");
  if (!nextBody || !content || !title) return false;
  content.innerHTML = nextBody.innerHTML;
  const heading = content.querySelector("h1, h2");
  title.textContent = heading?.textContent || nextDocument.title || "Edit";
  heading?.closest(".page-heading")?.classList.add("dialog-page-heading");
  syncConditionalFields();
  requestAnimationFrame(() => content.querySelector("[autofocus], input:not([type=hidden]), select, button")?.focus());
  return true;
}

async function openAppDialog(url, returnPath) {
  const dialog = document.getElementById("app-dialog");
  if (!dialog) {
    navigate(url);
    return;
  }
  const epoch = ++requestEpoch;
  if (navigationController) navigationController.abort();
  if (refreshController) refreshController.abort();
  navigationController = new AbortController();
  interactionInFlight = true;
  setProgress(true);
  setSyncState("Loading", true);
  try {
    const result = await fetchDocument(url, { signal: navigationController.signal });
    if (epoch !== requestEpoch) return;
    dialog.dataset.returnPath = returnPath || window.location.pathname;
    if (updateAppDialog(result.document)) {
      if (!dialog.open) dialog.showModal();
      setSyncState("Live");
    } else {
      window.location.href = result.response.url || url;
    }
  } catch (error) {
    if (error.name !== "AbortError" && epoch === requestEpoch) {
      toast("Couldn’t open that editor.", "error");
      setSyncState("Offline");
    }
  } finally {
    if (epoch === requestEpoch) {
      interactionInFlight = false;
      setProgress(false);
    }
  }
}

function submitReorder(item, target, position) {
  if (!item || !target || item === target) return;
  const form = document.createElement("form");
  form.method = "post";
  form.action = item.dataset.reorderUrl;
  form.hidden = true;
  for (const [name, value] of [
    ["source", item.dataset.reorderId],
    ["target", target.dataset.reorderId],
    ["position", position]
  ]) {
    const input = document.createElement("input");
    input.name = name;
    input.value = value;
    form.appendChild(input);
  }
  if (position === "before") target.before(item);
  else target.after(item);
  document.body.appendChild(form);
  form.requestSubmit();
}

function showScheduleEditor(start, end, x, y) {
  const editor = document.getElementById("schedule-editor");
  const input = document.getElementById("schedule-limit");
  if (!editor || !input) return;
  const low = Math.min(start, end);
  const high = Math.max(start, end);
  setScheduleSelection(low, high);
  editor.querySelector('[name="start_hour"]').value = low;
  editor.querySelector('[name="end_hour"]').value = high + 1;
  const count = high - low + 1;
  document.getElementById("schedule-selection-label").textContent =
    `${String(low).padStart(2, "0")}:00–${String(high + 1).padStart(2, "0")}:00 · ${count} hour${count === 1 ? "" : "s"}`;
  const selected = selectedScheduleHours();
  const limits = new Set(selected.map((hour) => hour.dataset.limit));
  input.value = limits.size === 1 ? selected[0].dataset.limit : "";
  const left = Math.max(16, Math.min(window.innerWidth - 366, x - 175));
  const top = Math.max(16, Math.min(window.innerHeight - 290, y + 14));
  editor.style.left = `${left}px`;
  editor.style.top = `${top}px`;
  if (typeof editor.showPopover === "function") {
    if (!editor.matches(":popover-open")) editor.showPopover();
  } else {
    editor.classList.add("open");
  }
  updateSchedulePreview();
  requestAnimationFrame(() => input.focus());
}

document.addEventListener("pointerdown", (event) => {
  const hour = event.target.closest("[data-schedule-hour]");
  if (hour && event.button === 0) {
    event.preventDefault();
    const value = Number(hour.dataset.scheduleHour);
    scheduleDrag = { start: value, end: value, x: event.clientX, y: event.clientY };
    setScheduleSelection(value, value);
    return;
  }
  if (!event.target.closest("#schedule-editor")) {
    closeScheduleEditor();
  }
});

document.addEventListener("pointermove", (event) => {
  if (!scheduleDrag) return;
  const hour = document.elementFromPoint(event.clientX, event.clientY)?.closest("[data-schedule-hour]");
  if (!hour) return;
  scheduleDrag.end = Number(hour.dataset.scheduleHour);
  scheduleDrag.x = event.clientX;
  scheduleDrag.y = event.clientY;
  setScheduleSelection(
    Math.min(scheduleDrag.start, scheduleDrag.end),
    Math.max(scheduleDrag.start, scheduleDrag.end)
  );
});

document.addEventListener("pointerup", () => {
  if (!scheduleDrag) return;
  showScheduleEditor(scheduleDrag.start, scheduleDrag.end, scheduleDrag.x, scheduleDrag.y);
  scheduleDrag = undefined;
});

document.addEventListener("pointercancel", () => {
  scheduleDrag = undefined;
  clearScheduleSelection();
});

document.addEventListener("dragstart", (event) => {
  const item = event.target.closest("[data-reorder-kind]");
  if (!item) return;
  draggedItem = item;
  item.classList.add("is-dragging");
  event.dataTransfer.effectAllowed = "move";
  event.dataTransfer.setData("text/plain", item.dataset.reorderId);
});

document.addEventListener("dragover", (event) => {
  if (!draggedItem) return;
  const target = event.target.closest(`[data-reorder-kind="${draggedItem.dataset.reorderKind}"]`);
  if (!target || target === draggedItem) return;
  event.preventDefault();
  const rect = target.getBoundingClientRect();
  const position = event.clientY < rect.top + rect.height / 2 ? "before" : "after";
  for (const item of document.querySelectorAll(".drop-before, .drop-after")) {
    item.classList.remove("drop-before", "drop-after");
  }
  target.classList.add(position === "before" ? "drop-before" : "drop-after");
});

document.addEventListener("drop", (event) => {
  if (!draggedItem) return;
  const target = event.target.closest(`[data-reorder-kind="${draggedItem.dataset.reorderKind}"]`);
  if (!target || target === draggedItem) return;
  event.preventDefault();
  const position = target.classList.contains("drop-before") ? "before" : "after";
  submitReorder(draggedItem, target, position);
});

document.addEventListener("dragend", () => {
  draggedItem?.classList.remove("is-dragging");
  for (const item of document.querySelectorAll(".drop-before, .drop-after")) {
    item.classList.remove("drop-before", "drop-after");
  }
  draggedItem = undefined;
});

document.addEventListener("click", (event) => {
  if (event.target.matches?.("#app-dialog")) {
    closeAppDialog();
    return;
  }
  const dialogClose = event.target.closest("[data-dialog-close]");
  if (dialogClose) {
    closeAppDialog();
    return;
  }
  const dialogLink = event.target.closest("a[data-dialog]");
  if (dialogLink) {
    event.preventDefault();
    openAppDialog(dialogLink.href, dialogLink.dataset.dialogReturn || window.location.pathname);
    return;
  }
  const linkInsideDialog = event.target.closest("#app-dialog a[href]");
  if (linkInsideDialog) {
    const dialog = document.getElementById("app-dialog");
    const returnPath = new URL(dialog.dataset.returnPath || "/", window.location.href);
    const destination = new URL(linkInsideDialog.href, window.location.href);
    if (destination.pathname === returnPath.pathname) {
      event.preventDefault();
      closeAppDialog();
      return;
    }
  }
  const close = event.target.closest("[data-close-schedule]");
  if (close) {
    closeScheduleEditor();
    return;
  }
  const preset = event.target.closest("[data-speed-preset]");
  if (preset) {
    const input = document.getElementById("schedule-limit");
    if (input) {
      input.value = preset.dataset.speedPreset;
      input.dispatchEvent(new Event("input", { bubbles: true }));
      input.focus();
    }
    return;
  }
  const keyboardHour = event.target.closest("[data-schedule-hour]");
  if (keyboardHour && event.detail === 0) {
    const value = Number(keyboardHour.dataset.scheduleHour);
    const rect = keyboardHour.getBoundingClientRect();
    showScheduleEditor(value, value, rect.left + rect.width / 2, rect.bottom);
    return;
  }
  const link = event.target.closest("a[href]");
  if (!link || event.defaultPrevented || event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
  if (link.target || link.hasAttribute("download")) return;
  const url = new URL(link.href, window.location.href);
  if (url.origin !== window.location.origin) return;
  event.preventDefault();
  navigate(url.toString());
});

document.addEventListener("submit", async (event) => {
  const form = event.target;
  if (!(form instanceof HTMLFormElement)) return;
  event.preventDefault();
  if (!form.reportValidity()) return;
  const method = (form.method || "get").toUpperCase();
  if (method === "GET") {
    navigate(formUrl(form), "push", true);
    return;
  }

  const button = event.submitter || form.querySelector('button[type="submit"], button:not([type])');
  const dialog = form.closest("#app-dialog");
  const row = form.closest(".download-item, tr");
  const epoch = ++requestEpoch;
  if (navigationController) navigationController.abort();
  if (refreshController) refreshController.abort();
  navigationController = new AbortController();
  interactionInFlight = true;
  button?.setAttribute("disabled", "");
  button?.classList.add("is-busy");
  row?.classList.add("is-busy");
  setProgress(true);
  setSyncState("Saving", true);
  try {
    const body = new URLSearchParams(new FormData(form));
    const result = await fetchDocument(form.action || window.location.href, {
      method,
      body,
      signal: navigationController.signal,
      headers: { "Content-Type": "application/x-www-form-urlencoded;charset=UTF-8" }
    });
    if (epoch !== requestEpoch) return;
    const finalUrl = result.response.url || window.location.href;
    const finalPath = new URL(finalUrl, window.location.href).pathname;
    const returnPath = dialog
      ? new URL(dialog.dataset.returnPath || "/", window.location.href).pathname
      : null;
    if (dialog && finalPath !== returnPath) {
      updateAppDialog(result.document);
      setSyncState("Live");
    } else if (applyDocument(result.document, finalUrl, "replace")) {
      if (dialog) closeAppDialog();
      setSyncState("Saved");
      toast(form.action.endsWith("/logout") ? "Signed out" : "Updated");
      setTimeout(() => setSyncState("Live"), 900);
    }
  } catch (error) {
    if (error.name !== "AbortError" && epoch === requestEpoch) {
      setSyncState("Offline");
      toast("That change didn’t go through. Please try again.", "error");
      button?.removeAttribute("disabled");
      button?.classList.remove("is-busy");
      row?.classList.remove("is-busy");
    }
  } finally {
    if (epoch === requestEpoch) {
      interactionInFlight = false;
      setProgress(false);
    }
  }
});

document.addEventListener("input", (event) => {
  if (event.target.id === "schedule-limit") {
    updateSchedulePreview();
  }
  if (event.target.matches("[data-speed-input]")) {
    const parsed = parseSpeedInput(event.target.value);
    event.target.setCustomValidity(
      !parsed && event.target.value.trim()
        ? "Use a speed such as 500K, 10M, 25 MB/s, or unlimited."
        : ""
    );
  }
  const form = event.target.closest("form");
  if (!form) return;
  form.classList.add("is-dirty");
  if (!form.matches("[data-live-form]")) return;
  clearTimeout(liveFormTimer);
  liveFormTimer = setTimeout(() => navigate(formUrl(form), "replace", true), 260);
});

document.addEventListener("change", (event) => {
  const form = event.target.closest("form");
  if (!form) return;
  form.classList.add("is-dirty");
  syncConditionalFields();
  if (form.matches("[data-live-submit]")) {
    form.requestSubmit();
    return;
  }
  if (form.matches("[data-live-form]")) {
    clearTimeout(liveFormTimer);
    navigate(formUrl(form), "replace", true);
  }
});

window.addEventListener("popstate", () => navigate(window.location.href, "none"));
window.addEventListener("online", () => { setSyncState("Live"); subtleRefresh(); });
window.addEventListener("offline", () => setSyncState("Offline"));
window.addEventListener("wheel", () => pauseBackgroundRefresh(), { passive: true });
window.addEventListener("pointerdown", () => pauseBackgroundRefresh(), { passive: true });
window.addEventListener("touchstart", () => pauseBackgroundRefresh(), { passive: true });
window.visualViewport?.addEventListener("resize", () => pauseBackgroundRefresh(1000));
window.addEventListener("keydown", (event) => {
  if (event.key === "Escape") closeScheduleEditor();
  const handle = event.target.closest("[data-drag-handle]");
  if (!handle || !["ArrowUp", "ArrowDown"].includes(event.key)) return;
  event.preventDefault();
  const item = handle.closest("[data-reorder-kind]");
  const peers = [...document.querySelectorAll(`[data-reorder-kind="${item.dataset.reorderKind}"]`)];
  const index = peers.indexOf(item);
  const target = event.key === "ArrowUp" ? peers[index - 1] : peers[index + 1];
  if (target) {
    submitReorder(item, target, event.key === "ArrowUp" ? "before" : "after");
  }
});
document.getElementById("app-dialog")?.addEventListener("close", () => {
  document.getElementById("dialog-content")?.replaceChildren();
});

if (pairingStatus) {
  async function probeExistingSession() {
    try {
      const response = await fetch("/api/session", { credentials: "same-origin" });
      if (response.status === 204) {
        window.location.href = loginNext;
        return true;
      }
    } catch (_) {
    }
    return false;
  }

  probeExistingSession().then((handled) => {
    if (handled) {
      return;
    }
    if (loginLoading) {
      loginLoading.classList.add("hidden");
    }
    if (loginPairing) {
      loginPairing.classList.remove("hidden");
    }
    setInterval(async () => {
      try {
        const response = await fetch("/login/status", { credentials: "same-origin" });
        const data = await response.json();
        if (data.status === "approved") {
          window.location.href = loginNext;
        } else if (data.status === "expired") {
          pairingStatus.textContent = "Pairing expired. Reloading...";
          window.location.reload();
        }
      } catch (_) {
        pairingStatus.textContent = "Waiting for daemon...";
      }
    }, 1200);
  });
}

async function subtleRefresh() {
  if (refreshInFlight || interactionInFlight || draggedItem || document.hidden || Date.now() < refreshPausedUntil) return;
  refreshInFlight = true;
  const epoch = requestEpoch;
  refreshController = new AbortController();
  try {
    const result = await fetchDocument(window.location.href, { signal: refreshController.signal });
    if (epoch !== requestEpoch) return;
    const doc = result.document;
    const nextHeader = doc.getElementById("app-header");
    const currentHeader = document.getElementById("app-header");
    if (!nextHeader || !currentHeader) {
      window.location.href = "/login";
      return;
    }
    const editing = document.querySelector("form.is-dirty:not([data-live-form]), input:focus, textarea:focus, select:focus");
    if (editing) {
      const headerPatch = reconcileElement(currentHeader, nextHeader, true);
      window.__ariatuiLastPatch = { body: { skipped: "editing" }, header: headerPatch };
    } else {
      applyDocument(doc, window.location.href, "none", true);
    }
    setSyncState("Live");
  } catch (error) {
    if (error.name !== "AbortError" && epoch === requestEpoch) {
      setSyncState("Reconnecting");
    }
  } finally {
    refreshInFlight = false;
    refreshController = undefined;
  }
}

if (document.body.dataset.autorefresh === "1") {
  setInterval(subtleRefresh, 1800);
}
syncConditionalFields();
"##;
    script.replace(
        "__LOGIN_NEXT__",
        &serde_json::to_string(&normalize_next_path(login_next)).unwrap(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::{
        path::Path,
        sync::Arc,
        time::{Duration, Instant, SystemTime, UNIX_EPOCH},
    };

    use axum::{
        body::{Body, to_bytes},
        http::{Request, StatusCode, header},
    };
    use tower::ServiceExt;

    use crate::{
        config::AppConfig,
        daemon::{AppContext, DaemonState},
        paths::AppPaths,
        state::PersistedState,
        web::{
            AUTH_COOKIE_NAME, approve_pairing_pin, create_or_get_pairing,
            issue_session_cookie_value,
        },
    };

    fn test_paths(root: &Path) -> AppPaths {
        let config_dir = root.join("config");
        let state_dir = root.join("state");
        let runtime_dir = root.join("runtime");
        let user_service_dir = config_dir.join("systemd/user");
        AppPaths {
            config_dir: config_dir.clone(),
            state_dir: state_dir.clone(),
            runtime_dir: runtime_dir.clone(),
            config_file: config_dir.join("config.toml"),
            state_file: state_dir.join("state.toml"),
            socket_path: runtime_dir.join("daemon.sock"),
            daemon_marker_file: runtime_dir.join(".daemon"),
            snapshot_cache_file: runtime_dir.join(".snapshot"),
            history_file: state_dir.join("history.json"),
            aria2_session_file: state_dir.join("aria2.session"),
            retry_state_file: state_dir.join("retry-state.json"),
            user_service_dir: user_service_dir.clone(),
            user_service_file: user_service_dir.join("ariatui-daemon.service"),
            system_service_file: root.join("ariatui-daemon.service"),
        }
    }

    async fn test_state(name: &str) -> SharedDaemonState {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("ariatui-web-tests-{name}-{nonce}"));
        let paths = test_paths(&root);
        let app = Arc::new(AppContext::new(
            paths,
            AppConfig::default(),
            PersistedState::default(),
            "/tmp/ariatui".into(),
            "test-build".into(),
        ));
        Arc::new(DaemonState::new(app).await.unwrap())
    }

    async fn response_json(response: Response) -> serde_json::Value {
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&body).unwrap()
    }

    async fn response_text(response: Response) -> String {
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        String::from_utf8(body.to_vec()).unwrap()
    }

    #[tokio::test]
    async fn current_page_renders_reactive_accessible_app_shell() {
        let state = test_state("reactive-shell").await;
        let app = router(state.clone());
        let token = issue_session_cookie_value(state.as_ref(), 30)
            .await
            .unwrap();

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/current")
                    .header(header::COOKIE, format!("{AUTH_COOKIE_NAME}={token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let html = response_text(response).await;
        assert!(html.contains(r#"class="app-shell""#));
        assert!(html.contains(r#"aria-label="Main navigation""#));
        assert!(html.contains(r#"data-live-form"#));
        assert!(!html.contains("startViewTransition"));
        assert!(html.contains("aria-live=\"polite\""));
        assert!(html.contains(r#"class="speed-control""#));
        assert!(html.contains("color-scheme: dark"));
        assert!(html.contains("function reconcileElement"));
        assert!(html.contains("const sameView = nextPath === window.location.pathname"));
        assert!(!html.contains("currentHeader.replaceWith(nextHeader)"));
        let navigate_script = html
            .split("async function navigate")
            .nth(1)
            .unwrap()
            .split("function formUrl")
            .next()
            .unwrap();
        assert!(navigate_script.contains("}, 220);"));
        assert!(!navigate_script.contains("setSyncState(\"Loading\""));
        assert!(!html.contains(">Apply</button>"));
    }

    #[tokio::test]
    async fn scheduler_page_renders_direct_manipulation_timeline() {
        let state = test_state("schedule-timeline").await;
        let app = router(state.clone());
        let token = issue_session_cookie_value(state.as_ref(), 30)
            .await
            .unwrap();

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/scheduler")
                    .header(header::COOKIE, format!("{AUTH_COOKIE_NAME}={token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let html = response_text(response).await;
        assert_eq!(html.matches("data-schedule-hour=").count(), 24);
        assert!(html.contains(r#"popover="manual""#));
        assert!(html.contains("Drag across hours"));
        assert!(html.contains("Follow schedule"));
        assert!(html.contains("Unlimited for now"));
        assert!(!html.contains("<th>Start</th>"));
    }

    #[test]
    fn queue_items_use_dialogs_and_direct_drag_ordering() {
        let mut snapshot = Snapshot::empty(
            "socket".into(),
            "state".into(),
            "config".into(),
            "binary".into(),
            "build".into(),
        );
        snapshot.current_downloads = ["first", "second"]
            .into_iter()
            .map(|gid| DownloadItem {
                gid: gid.into(),
                status: DownloadStatus::Waiting,
                name: format!("{gid}.iso"),
                primary_path: None,
                source_uri: None,
                info_hash: None,
                num_seeders: None,
                followed_by: Vec::new(),
                belongs_to: None,
                is_metadata_only: false,
                total_bytes: 100,
                completed_bytes: 10,
                download_speed_bps: 0,
                realtime_download_speed_bps: 0,
                upload_speed_bps: 0,
                eta_seconds: None,
                connections: None,
                error_code: None,
                error_message: None,
            })
            .collect();
        let query = CurrentListQuery::from_query(&ItemQuery::default());

        let html = render_current_page(&snapshot, &query, None, None, true);

        assert_eq!(html.matches(r#"data-reorder-kind="download""#).count(), 2);
        assert!(html.contains(r#"data-dialog-return="/current"#));
        assert!(html.contains(r#"id="app-dialog""#));
        assert!(!html.contains("Move up"));
        assert!(!html.contains("Move down"));
    }

    #[tokio::test]
    async fn api_session_accepts_bearer_and_cookie_auth() {
        let state = test_state("api-auth").await;
        let app = router(state.clone());
        let token = issue_session_cookie_value(state.as_ref(), 30)
            .await
            .unwrap();

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/session")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/session")
                    .header(header::COOKIE, format!("{AUTH_COOKIE_NAME}={token}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    #[tokio::test]
    async fn pairing_api_reports_pending_approved_and_expired_states() {
        let state = test_state("pairing-states").await;
        let app = router(state.clone());

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/pairings")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let created = response_json(response).await;
        let request_id = created["request_id"].as_str().unwrap().to_string();
        let pin = created["pin"].as_str().unwrap().to_string();
        assert_eq!(created["expires_in_secs"].as_u64(), Some(PAIRING_TTL_SECS));

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/pairings/{request_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response_json(response).await["status"], "pending");

        approve_pairing_pin(state.as_ref(), &pin).await.unwrap();

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/api/pairings/{request_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let approved = response_json(response).await;
        assert_eq!(approved["status"], "approved");
        assert!(approved["auth_token"].as_str().unwrap().starts_with("v1."));
        assert!(approved["expires_in_secs"].as_u64().unwrap() > 0);

        let (expired_request_id, _) = create_or_get_pairing(state.as_ref(), None).await.unwrap();
        state
            .web_pairings
            .lock()
            .await
            .get_mut(&expired_request_id)
            .unwrap()
            .expires_at = Instant::now() - Duration::from_secs(1);

        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/pairings/{expired_request_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response_json(response).await["status"], "expired");
    }

    #[tokio::test]
    async fn unauthenticated_api_routes_return_json_401_without_redirects() {
        let state = test_state("unauthenticated").await;
        let app = router(state);

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/session")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(response.headers().get(header::LOCATION).is_none());
        assert_eq!(
            response_json(response).await["error"],
            "authentication required"
        );

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/downloads")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(r#"{"url":"https://example.com/file.iso"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert!(response.headers().get(header::LOCATION).is_none());
        assert_eq!(
            response_json(response).await["error"],
            "authentication required"
        );
    }

    #[test]
    fn prepared_download_helper_auto_selects_remote_filename_for_api_queue() {
        let prepared = prepared_download_from_resolved(ResolvedHttpUrl {
            url: "https://example.com/download".into(),
            url_filename: "download".into(),
            remote_filename: Some("server-name.iso".into()),
            redirect_filename: None,
            final_url: Some("https://cdn.example.com/server-name.iso".into()),
            is_torrent: false,
        });

        let queued = prepared.into_api_queue();
        assert_eq!(queued.filename.as_deref(), Some("server-name.iso"));
        assert_eq!(queued.display_name, "server-name.iso");
        assert_eq!(
            queued.final_url.as_deref(),
            Some("https://cdn.example.com/server-name.iso")
        );
    }

    #[test]
    fn prompt_download_requires_filename_for_queue_submission() {
        let prepared = prepared_download_from_resolved(ResolvedHttpUrl {
            url: "https://example.com/download".into(),
            url_filename: "download".into(),
            remote_filename: Some("server-name.iso".into()),
            redirect_filename: None,
            final_url: Some("https://cdn.example.com/server-name.iso".into()),
            is_torrent: false,
        });

        assert!(prepared.clone().into_queue_with_filename(None).is_err());
        let queued = prepared
            .into_queue_with_filename(Some("custom-name.iso".into()))
            .unwrap();
        assert_eq!(queued.filename.as_deref(), Some("custom-name.iso"));
        assert_eq!(queued.display_name, "custom-name.iso");
    }
}
