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
use chrono::{Duration, Local, Timelike};
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
        .route("/torrents", get(torrents_page).post(save_torrents))
        .route("/routing", get(routing_page))
        .route("/routing/rule/new", get(new_rule_page))
        .route("/routing/rule/{index}/edit", get(edit_rule_page))
        .route("/routing/rule/save", post(save_rule))
        .route("/routing/rule/{index}/delete", post(delete_rule))
        .route("/routing/rule/{index}/move/up", post(move_rule_up))
        .route("/routing/rule/{index}/move/down", post(move_rule_down))
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
            Self::Current => "Current",
            Self::History => "History",
            Self::Scheduler => "Scheduler",
            Self::Torrents => "Torrents",
            Self::Routing => "Routing",
            Self::Webhooks => "Webhooks",
            Self::WebUi => "Web UI",
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
struct RangeFormData {
    start_hour: usize,
    end_hour: usize,
    limit: String,
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
<h2>Browser pairing</h2>
<p>Type this PIN into the terminal UI in the <strong>Web UI</strong> tab to approve this browser:</p>
<p class="pin">{}</p>
<p class="muted">The page will continue automatically after approval.</p>
<div id="pairing-status" class="muted">Waiting for terminal approval...</div>
</section>"#,
        esc(pin)
    );
    render_public_shell("Login", &body, Some(next))
}

fn render_public_shell(title: &str, body: &str, login_next: Option<&str>) -> String {
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
<h1>AriaTUI Web</h1>
{}
</main>
<script>{}</script>
</body>
</html>"#,
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
        let _ = write!(
            tabs,
            r#"<a class="{class}" href="{}">{}</a>"#,
            tab.href(),
            esc(tab.title())
        );
    }
    format!(
        r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{}</title>
<style>{}</style>
</head>
<body data-autorefresh="{}">
<main class="wrap">
<header id="app-header" class="header">{}</header>
<nav class="tabs">{}</nav>
<section id="page-body">{}</section>
<form method="post" action="/logout" class="logout"><button type="submit">Log out</button></form>
</main>
<script>{}</script>
</body>
</html>"#,
        esc(page_title),
        styles(),
        if auto_refresh { "1" } else { "0" },
        render_header(snapshot),
        tabs,
        body,
        script(None)
    )
}

fn render_header(snapshot: &Snapshot) -> String {
    format!(
        "<strong>aria2 {}</strong> &nbsp; down {} &nbsp; up {} &nbsp; active {} waiting {} stopped {} &nbsp; mode {:?} limit {} &nbsp; web {}",
        esc(&format!("{:?}", snapshot.aria2_status.lifecycle).to_lowercase()),
        esc(&format_bytes_per_sec(snapshot.global.download_speed_bps)),
        esc(&format_bytes_per_sec(snapshot.global.upload_speed_bps)),
        snapshot.global.num_active,
        snapshot.global.num_waiting,
        snapshot.global.num_stopped,
        snapshot.scheduler.mode,
        esc(&format_limit(snapshot.scheduler.effective_limit_bps)),
        esc(&format!("{:?}", snapshot.web_ui.status).to_lowercase()),
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
        let selected_class = if Some(item.gid.as_str()) == selected_gid {
            "selected"
        } else {
            ""
        };
        let item_query =
            current_query_suffix(Some(&item.gid), &query.search, query.filter, query.sort);
        let actions = match item.status {
            DownloadStatus::Active => format!(
                r#"<form method="post" action="/current/{gid}/pause{query}"><button>Pause</button></form><a class="button danger" href="/current/{gid}/cancel{query}">Cancel</a>"#,
                gid = esc(&item.gid),
                query = esc(&item_query),
            ),
            DownloadStatus::Paused => format!(
                r#"<form method="post" action="/current/{gid}/resume{query}"><button>Resume</button></form><form method="post" action="/current/{gid}/move/up{query}"><button>Up</button></form><form method="post" action="/current/{gid}/move/down{query}"><button>Down</button></form><a class="button danger" href="/current/{gid}/cancel{query}">Cancel</a>"#,
                gid = esc(&item.gid),
                query = esc(&item_query),
            ),
            DownloadStatus::Waiting => format!(
                r#"<form method="post" action="/current/{gid}/move/up{query}"><button>Up</button></form><form method="post" action="/current/{gid}/move/down{query}"><button>Down</button></form><a class="button danger" href="/current/{gid}/cancel{query}">Cancel</a>"#,
                gid = esc(&item.gid),
                query = esc(&item_query),
            ),
            _ => String::new(),
        };
        let _ = write!(
            rows,
            r#"<tr class="{selected_class}">
<td><a href="/current{}">{}</a></td>
<td>{}</td>
<td>{}</td>
<td>{} / {}</td>
<td>{}</td>
<td>{}</td>
<td>{}</td>
<td class="actions">{}</td>
</tr>"#,
            esc(&current_query_suffix(
                Some(&item.gid),
                &query.search,
                query.filter,
                query.sort,
            )),
            esc(status_label(&item.status)),
            esc(&item.name),
            esc(&progress_text(item)),
            esc(&format_bytes(item.completed_bytes)),
            esc(&format_bytes(item.total_bytes)),
            esc(&format_bytes_per_sec(item.download_speed_bps)),
            esc(&format_eta(item.eta_seconds)),
            esc(&item.gid),
            actions,
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
        r#"<div class="toolbar">
<form method="get" action="/current" class="inline">
<input type="text" name="search" value="{search}" placeholder="Search name, GID, path, source">
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
<button type="submit">Apply</button>
<a class="button" href="/current">Clear</a>
</form>
<form method="post" action="/current/pause-all{query}"><button>Pause all</button></form>
<form method="post" action="/current/resume-all{query}"><button>Resume all</button></form>
<a class="button" href="/current/add">Add URI</a>
</div>
<p class="muted">Visible {visible_count} of {total_count}. Up/Down reorder waiting or paused items.</p>"#,
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
        r#"<section class="card">
<h2>Current downloads</h2>
<div class="table-wrap">
<table>
<thead><tr><th>Status</th><th>Name</th><th>Progress</th><th>Done/Total</th><th>Speed</th><th>ETA</th><th>GID</th><th>Actions</th></tr></thead>
<tbody>{}</tbody>
</table>
</div>
</section>"#,
        rows
    );
    let _ = write!(
        body,
        r#"<aside class="card"><h2>Details</h2>{}</aside>"#,
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
        r#"<section class="card narrow-card">
<h2>Add URI</h2>
<form method="post" action="/current/add/resolve" class="stack">
<label>HTTP, HTTPS, FTP, SFTP, or magnet URI</label>
<input type="text" name="url" value="{}" placeholder="https://example.com/file.iso or magnet:?..." />
<div class="actions"><button type="submit">Add</button><a class="button" href="/current">Back</a></div>
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
            r#"<section class="card narrow-card">
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
        r#"<section class="card narrow-card">
<h2>Cancel download</h2>
<p>{}</p>
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
<div class="actions"><button type="submit" class="danger">Cancel download</button><a class="button" href="/current">Back</a></div>
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
        let _ = write!(
            rows,
            r#"<tr>
<td><a href="/history{}">{}</a></td>
<td>{}</td>
<td>{}</td>
<td>{}</td>
<td>{}</td>
<td><form method="post" action="/history/{}/remove{}"><button>Forget</button></form></td>
</tr>"#,
            esc(&item_query),
            esc(status_label(&item.status)),
            esc(&item.name),
            esc(&format_bytes(item.total_bytes)),
            esc(item.error_code.as_deref().unwrap_or("-")),
            esc(&item.gid),
            esc(&item.gid),
            esc(&item_query),
        );
    }
    let history_query = history_query_suffix(selected_gid, &query.search, query.filter, query.sort);
    let body = format!(
        r#"<div class="toolbar">
<form method="get" action="/history" class="inline">
<input type="text" name="search" value="{search}" placeholder="Search name, GID, path, source, error">
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
<button type="submit">Apply</button>
<a class="button" href="/history">Clear</a>
</form>
<form method="post" action="/history/purge{history_query}"><button class="danger">Clear history</button></form>
</div>
<p class="muted">Visible {visible_count} of {total_count} history items.</p>
<div class="split">
<section class="card">
<h2>History</h2>
<div class="table-wrap">
<table>
<thead><tr><th>Status</th><th>Name</th><th>Size</th><th>Error</th><th>GID</th><th>Action</th></tr></thead>
<tbody>{rows}</tbody>
</table>
</div>
</section>
<aside class="card"><h2>Details</h2>{details}</aside>
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
    let mut rows = String::new();
    for range in &ranges {
        let _ = write!(
            rows,
            r#"<tr>
<td>{:02}:00</td><td>{:02}:00</td><td>{}</td>
<td class="actions">
<a class="button" href="/scheduler/range/{}/{}/edit">Edit</a>
<form method="post" action="/scheduler/range/delete">
<input type="hidden" name="start_hour" value="{}">
<input type="hidden" name="end_hour" value="{}">
<input type="hidden" name="limit" value="unlimited">
<button>Clear</button>
</form>
</td>
</tr>"#,
            range.0,
            range.1,
            esc(&format_limit(range.2)),
            range.0,
            range.1,
            range.0,
            range.1
        );
    }
    let mut body = String::new();
    if let Some(error) = error {
        let _ = write!(body, "<p class=\"error\">{}</p>", esc(error));
    }
    let _ = write!(
        body,
        r#"<div class="grid2">
<section class="card">
<h2>Scheduler</h2>
<form method="post" action="/scheduler/mode" class="inline">
<label><input type="radio" name="mode" value="manual" {}> Manual</label>
<label><input type="radio" name="mode" value="scheduled" {}> Scheduled</label>
<button type="submit">Apply mode</button>
</form>
<p>Manual limit: {} &nbsp; <a class="button" href="/scheduler/manual">Edit</a></p>
<p>Usual internet speed: {} &nbsp; <a class="button" href="/scheduler/usual">Edit</a></p>
<p>Effective limit: {}</p>
<p>Next change: {}</p>
<div class="chart-shell">{}</div>
</section>
<section class="card">
<div class="toolbar"><a class="button" href="/scheduler/range/new">New range</a></div>
<table>
<thead><tr><th>Start</th><th>End</th><th>Limit</th><th>Actions</th></tr></thead>
<tbody>{}</tbody>
</table>
</section>
</div>"#,
        if snapshot.scheduler.mode == ManualOrScheduled::Manual {
            "checked"
        } else {
            ""
        },
        if snapshot.scheduler.mode == ManualOrScheduled::Scheduled {
            "checked"
        } else {
            ""
        },
        esc(&format_limit(snapshot.scheduler.manual_limit_bps)),
        esc(&format_limit(snapshot.scheduler.usual_internet_speed_bps)),
        esc(&format_limit(snapshot.scheduler.effective_limit_bps)),
        esc(&snapshot.scheduler.next_change_at_local),
        render_schedule_svg(&snapshot.scheduler.schedule_limits_bps),
        rows,
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
        r#"<section class="card narrow-card">
<h2>Torrent Streaming</h2>
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
<div class="actions"><button type="submit">Save settings</button></div>
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
            format!(r#"<a class="button" href="/routing/rule/{index}/edit">Edit</a>"#)
        } else {
            format!(
                r#"<a class="button" href="/routing/rule/{index}/edit">Edit</a>
<form method="post" action="/routing/rule/{index}/move/up"><button>Up</button></form>
<form method="post" action="/routing/rule/{index}/move/down"><button>Down</button></form>
<form method="post" action="/routing/rule/{index}/delete"><button class="danger">Delete</button></form>"#
            )
        };
        let _ = write!(
            rows,
            r#"<tr><td>{}</td><td>{}</td><td>{}</td><td class="actions">{}</td></tr>"#,
            esc(kind),
            esc(&rule.pattern),
            esc(&rule.directory),
            actions
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
        r#"<div class="grid2">
<section class="card">
<h2>Routing</h2>
<p>Fallback folder: {}</p>
<div class="toolbar"><a class="button" href="/routing/rule/new">Add rule</a></div>
<table>
<thead><tr><th>Type</th><th>Pattern</th><th>Directory</th><th>Actions</th></tr></thead>
<tbody>{}</tbody>
</table>
</section>
<section class="card">
<h2>Rule tester</h2>
<form method="get" action="/routing" class="stack">
<input type="text" name="test" value="{}" placeholder="example-file.iso">
<button type="submit">Test</button>
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
        r#"<section class="card narrow-card">
<h2>Webhooks</h2>
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
<div class="actions"><button type="submit">Save</button></div>
</form>
<form method="post" action="/webhooks/test"><button type="submit">Send test notification</button></form>
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
        r#"<section class="card narrow-card">
<h2>Web UI</h2>
<p>Status: {:?}</p>
<p>URL: {}</p>
<p>Pairing auth: {}</p>
<p>Pending browser PINs: {}</p>
<p>Active browser sessions: {}</p>
{}
<form method="post" action="/web-ui" class="stack">
<label><input type="checkbox" name="enabled" {}> Enabled</label>
<label>Bind address</label>
<input type="text" name="bind_address" value="{}">
<label>Port</label>
<input type="number" name="port" min="1" max="65535" value="{}">
<label>Cookie lifetime (days)</label>
<input type="number" name="cookie_days" min="1" max="365" value="{}">
<div class="actions"><button type="submit">Save settings</button></div>
</form>
</section>"#,
        snapshot.web_ui.status,
        esc(&snapshot.web_ui.url),
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
        snapshot.web_ui.active_session_count,
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

fn render_schedule_svg(limits: &[Option<u64>; 24]) -> String {
    let finite = limits.iter().flatten().copied().collect::<Vec<_>>();
    let max_finite = finite.iter().copied().max().unwrap_or(1);
    let min_finite = finite.iter().copied().min().unwrap_or(max_finite);
    let current_hour = Local::now().hour() as usize;
    let chart_top = 16.0;
    let chart_height = 144.0;
    let bar_width = 14.0;
    let gap = 8.0;
    let mut body = String::new();
    body.push_str(
        r#"<svg class="schedule-chart" viewBox="0 0 584 220" role="img" aria-label="Hourly scheduler limits chart">"#,
    );
    body.push_str(r##"<rect x="0" y="0" width="584" height="220" rx="10" fill="#101010"/>"##);
    for grid in 0..=4 {
        let y = chart_top + (chart_height / 4.0) * grid as f64;
        let _ = write!(
            body,
            r##"<line x1="38" y1="{:.1}" x2="566" y2="{:.1}" stroke="#2b2b2b" stroke-width="1"/>"##,
            y, y
        );
    }
    for (hour, limit) in limits.iter().enumerate() {
        let x = 42.0 + hour as f64 * (bar_width + gap);
        let normalized = match limit {
            None => 1.0,
            Some(_) if max_finite == min_finite => 0.55,
            Some(value) => {
                ((*value - min_finite) as f64 / (max_finite - min_finite) as f64 * 0.85) + 0.15
            }
        };
        let bar_height = chart_height * normalized;
        let y = chart_top + chart_height - bar_height;
        let fill = if hour == current_hour {
            "#f2c94c"
        } else if limit.is_none() {
            "#25c2a0"
        } else {
            "#4f8cff"
        };
        let _ = write!(
            body,
            r#"<g><rect x="{:.1}" y="{:.1}" width="{:.1}" height="{:.1}" rx="3" fill="{}"><title>{:02}:00 - {}</title></rect></g>"#,
            x,
            y,
            bar_width,
            bar_height.max(2.0),
            fill,
            hour,
            esc(&format_limit(*limit))
        );
        if hour % 3 == 0 {
            let _ = write!(
                body,
                r##"<text x="{:.1}" y="188" text-anchor="middle" fill="#bdbdbd" font-size="11">{:02}</text>"##,
                x + bar_width / 2.0,
                hour
            );
        }
    }
    body.push_str(r##"<text x="18" y="18" fill="#bdbdbd" font-size="11">Higher bars mean higher limits. Unlimited uses full height.</text>"##);
    body.push_str(r##"<text x="18" y="206" fill="#8f8f8f" font-size="11">Hours</text>"##);
    body.push_str("</svg>");
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
body { font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; background: #111; color: #eee; margin: 0; }
.wrap { max-width: 1280px; margin: 0 auto; padding: 1rem; }
.narrow { max-width: 520px; }
.narrow-card { max-width: 720px; }
.header, .card, .tabs, .logout { border: 1px solid #444; background: #181818; padding: 0.75rem; margin-bottom: 0.75rem; }
.tabs { display: flex; gap: 0.5rem; flex-wrap: wrap; }
.tab, .button, button { background: #262626; color: #eee; border: 1px solid #555; text-decoration: none; padding: 0.45rem 0.7rem; display: inline-block; cursor: pointer; }
.tab.active { background: #0e5f5f; border-color: #29b8b8; }
.split { display: grid; grid-template-columns: 2fr 1fr; gap: 0.75rem; }
.grid2 { display: grid; grid-template-columns: 1fr 1fr; gap: 0.75rem; }
.stack { display: flex; flex-direction: column; gap: 0.6rem; }
.inline, .actions { display: flex; gap: 0.5rem; align-items: center; flex-wrap: wrap; }
.toolbar { margin-bottom: 0.75rem; }
.table-wrap { overflow-x: auto; }
table { width: 100%; border-collapse: collapse; }
th, td { border-bottom: 1px solid #333; padding: 0.45rem; text-align: left; vertical-align: top; }
input, select { width: 100%; box-sizing: border-box; background: #0f0f0f; color: #eee; border: 1px solid #555; padding: 0.45rem; }
.message { color: #7fe27f; }
.error { color: #ff8383; }
.muted { color: #bbb; }
.danger { border-color: #8a3d3d; }
.chart-shell { overflow-x: auto; }
.schedule-chart { width: 100%; min-width: 584px; height: auto; display: block; }
.details dt { color: #bbb; margin-top: 0.5rem; }
.details dd { margin-left: 0; margin-bottom: 0.35rem; word-break: break-word; }
.projection-shell { margin-top: 0.9rem; }
.projection-chart { width: 100%; height: auto; display: block; margin-bottom: 0.6rem; }
.phase-list { margin: 0; padding-left: 1.25rem; color: #ddd; }
.phase-list li { margin-bottom: 0.3rem; }
code { background: #0d0d0d; padding: 0.15rem 0.25rem; }
.pin { font-size: 2.4rem; font-weight: bold; letter-spacing: 0.25rem; text-align: center; margin: 1rem 0; color: #7fe27f; }
@media (max-width: 900px) { .split, .grid2 { grid-template-columns: 1fr; } }
"#
}

fn script(login_next: Option<&str>) -> String {
    let script = r#"
const loginNext = __LOGIN_NEXT__;
const pairingStatus = document.getElementById("pairing-status");
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
  try {
    const response = await fetch(window.location.href, {
      credentials: "same-origin",
      headers: { "X-Requested-With": "AriaTUI-WebRefresh" }
    });
    const text = await response.text();
    const doc = new DOMParser().parseFromString(text, "text/html");
    const nextHeader = doc.getElementById("app-header");
    const nextBody = doc.getElementById("page-body");
    const currentHeader = document.getElementById("app-header");
    const currentBody = document.getElementById("page-body");
    if (!nextHeader || !nextBody || !currentHeader || !currentBody) {
      window.location.href = "/login";
      return;
    }
    currentHeader.innerHTML = nextHeader.innerHTML;
    currentBody.innerHTML = nextBody.innerHTML;
  } catch (_) {
  }
}

if (document.body.dataset.autorefresh === "1") {
  setInterval(() => {
    if (!document.querySelector("input:focus, textarea:focus, select:focus")) {
      subtleRefresh();
    }
  }, 1500);
}
"#;
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
            aria2_session_file: state_dir.join("aria2.session"),
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
