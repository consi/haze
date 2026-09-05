//! HTTP API. `axum` routers + `utoipa` `OpenAPI` generation.

use axum::{Json, Router, extract::State, middleware as axum_mw, response::Html, routing::get};
use haze_probe::ProbeKind;
use haze_store::repo::settings as store_settings;
use serde::Serialize;
use utoipa::{
    Modify, OpenApi,
    openapi::security::{ApiKey, ApiKeyValue, HttpAuthScheme, HttpBuilder, SecurityScheme},
};
use uuid::Uuid;

mod admin_routes;
mod alerts_routes;
mod auth_routes;
mod error;
pub mod events_routes;
mod groups_routes;
mod hosts_routes;
mod metadata_routes;
mod middleware;
mod passkey_routes;
pub mod rate_limit;
pub mod replication_routes;
mod settings_routes;
pub mod state;
mod tree_routes;
mod user_routes;

pub use events_routes::ChangeKind;
pub use rate_limit::{LimiterHandle, SsePerIpMap, new_handle, new_sse_map};
pub use state::{AppState, ReplicationPool};

pub fn api_router(state: AppState) -> Router {
    let v1 = v1_router(&state).with_state(state.clone());

    Router::new()
        .nest("/v1", v1)
        .route("/openapi.json", get(openapi_json))
        .route("/docs", get(swagger_ui_html))
        .with_state(state)
}

/// Swagger UI page. The `OpenAPI` URL is built as an absolute path so a
/// trailing-slash visit (`/api/docs/`) resolves it correctly - otherwise
/// the relative `openapi.json` would resolve to
/// `/api/docs/openapi.json`, fall through to the SPA index.html, and
/// Swagger UI would render a "Parser error on line N / invalid version
/// field" page after trying to parse HTML as YAML. `state.cookie_path`
/// is the normalised `HAZE_BASE_URL` (empty in root mode), so the path
/// works for sub-path deployments too.
async fn swagger_ui_html(State(state): State<AppState>) -> Html<String> {
    let spec_url = format!("{}/api/openapi.json", state.cookie_path);
    Html(format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <title>Haze API</title>
    <link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/swagger-ui-dist@5/swagger-ui.css">
    <style>body{{margin:0;background:#fafbfc}}</style>
</head>
<body>
    <div id="swagger-ui"></div>
    <script src="https://cdn.jsdelivr.net/npm/swagger-ui-dist@5/swagger-ui-bundle.js"></script>
    <script>
        window.onload = () => {{
            window.ui = SwaggerUIBundle({{
                url: '{spec_url}',
                dom_id: '#swagger-ui',
                deepLinking: true,
                persistAuthorization: true
            }});
        }};
    </script>
</body>
</html>
"#
    ))
}

pub fn v1_router(state: &AppState) -> Router<AppState> {
    Router::new()
        .route("/probes", get(list_probes))
        .route("/server-info", get(server_info))
        .nest("/auth", auth_routes::router())
        .nest("/auth/passkey", passkey_routes::router())
        .nest("/user", user_routes::router())
        .nest("/groups", groups_routes::router())
        .nest("/hosts", hosts_routes::router())
        .nest("/tree", tree_routes::router())
        .nest("/settings", settings_routes::router())
        .nest("/admin", admin_routes::router())
        .nest("/alerts", alerts_routes::router())
        .nest("/events", events_routes::router())
        .nest("/replication", replication_routes::router())
        // Order matters: session_layer runs first to attach `CurrentUser`,
        // then rate_limit_layer reads that extension to bypass the limiter
        // for authenticated requests.
        .layer(axum_mw::from_fn_with_state(
            state.clone(),
            rate_limit::rate_limit_layer,
        ))
        .layer(axum_mw::from_fn_with_state(
            state.clone(),
            middleware::session_layer,
        ))
}

/// Registers the security schemes Swagger UI uses to populate its "Authorize"
/// dialog. We expose two: a Bearer token (`hzt_…`) and the session cookie
/// - they're both valid ways to authenticate API calls.
struct SecurityAddon;
impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi
            .components
            .get_or_insert_with(utoipa::openapi::Components::default);
        components.add_security_scheme(
            "bearerAuth",
            SecurityScheme::Http(
                HttpBuilder::new()
                    .scheme(HttpAuthScheme::Bearer)
                    .bearer_format("hzt_")
                    .description(Some(
                        "Personal access token created at /user. Format: hzt_<32 url-safe base64 bytes>.",
                    ))
                    .build(),
            ),
        );
        components.add_security_scheme(
            "sessionCookie",
            SecurityScheme::ApiKey(ApiKey::Cookie(ApiKeyValue::with_description(
                "haze_session",
                "Cookie set by POST /auth/login or POST /auth/passkey/login/finish.",
            ))),
        );
    }
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Haze API",
        version = "0.1.0",
        description = "Network latency monitor - host CRUD, time-series queries, auth."
    ),
    modifiers(&SecurityAddon),
    security(("bearerAuth" = []), ("sessionCookie" = [])),
    paths(
        list_probes,
        server_info,
        auth_routes::login,
        auth_routes::logout,
        auth_routes::me,
        groups_routes::list,
        groups_routes::get_one,
        groups_routes::create,
        groups_routes::update,
        groups_routes::delete,
        hosts_routes::list,
        hosts_routes::get_one,
        hosts_routes::create,
        hosts_routes::update,
        hosts_routes::delete,
        hosts_routes::series,
        replication_routes::slot_metadata,
        metadata_routes::history,
        metadata_routes::detail,
        tree_routes::tree,
        settings_routes::get_storage,
        settings_routes::update_storage,
        settings_routes::get_workers,
        settings_routes::update_workers,
        settings_routes::get_alerting,
        settings_routes::update_alerting,
        settings_routes::get_host_defaults,
        settings_routes::update_host_defaults,
        settings_routes::get_public_mode,
        settings_routes::update_public_mode,
        admin_routes::list_users,
        admin_routes::create_user,
        admin_routes::update_user,
        admin_routes::reset_password,
        admin_routes::delete_user,
        admin_routes::restart,
        passkey_routes::register_begin,
        passkey_routes::register_finish,
        passkey_routes::login_begin,
        passkey_routes::login_finish,
        user_routes::change_password,
        user_routes::list_passkeys,
        user_routes::delete_passkey,
        user_routes::list_tokens,
        user_routes::create_token,
        user_routes::delete_token,
        alerts_routes::list_rules,
        alerts_routes::get_rule,
        alerts_routes::create_rule,
        alerts_routes::update_rule,
        alerts_routes::delete_rule,
        alerts_routes::list_states,
        alerts_routes::list_webhooks,
        alerts_routes::create_webhook,
        alerts_routes::update_webhook,
        alerts_routes::delete_webhook,
        alerts_routes::test_webhook,
        replication_routes::instance_info,
        replication_routes::list_peers,
        replication_routes::get_peer,
        replication_routes::create_peer,
        replication_routes::update_peer,
        replication_routes::delete_peer,
        replication_routes::test_peer,
        replication_routes::peer_groups_preview,
        replication_routes::list_rules,
        replication_routes::create_rule,
        replication_routes::update_rule,
        replication_routes::delete_rule,
        replication_routes::list_inbound,
        replication_routes::delete_inbound,
        replication_routes::unblock_inbound,
        replication_routes::upsert_slot,
        replication_routes::delete_slot_route,
        replication_routes::slot_manifest,
        replication_routes::slot_range,
        replication_routes::slot_ack,
        replication_routes::slot_stream,
    ),
    components(schemas(
        ProbeDescriptor,
        ServerInfo,
        auth_routes::LoginReq,
        auth_routes::LoginResp,
        groups_routes::CreateReq,
        groups_routes::UpdateReq,
        hosts_routes::CreateReq,
        hosts_routes::UpdateHostReq,
        hosts_routes::HostResp,
        hosts_routes::SeriesPoint,
        hosts_routes::SeriesResp,
        tree_routes::TreeResp,
        passkey_routes::BeginResp,
        passkey_routes::RegisterFinishReq,
        passkey_routes::LoginFinishReq,
        passkey_routes::LoginFinishResp,
        user_routes::ChangePasswordReq,
        user_routes::PasskeyResp,
        user_routes::CreateTokenReq,
        user_routes::CreateTokenResp,
        user_routes::TokenResp,
        groups_routes::GroupResp,
        haze_store::repo::hosts::Host,
        settings_routes::StorageSettingsResp,
        settings_routes::UpdateStorageSettingsReq,
        settings_routes::WorkerSettingsResp,
        settings_routes::UpdateWorkerSettingsReq,
        settings_routes::AlertingSettingsResp,
        settings_routes::UpdateAlertingSettingsReq,
        settings_routes::HostDefaultsResp,
        settings_routes::UpdateHostDefaultsReq,
        settings_routes::PublicModeSettingsResp,
        settings_routes::UpdatePublicModeSettingsReq,
        haze_store::AlertingSettings,
        haze_store::HostDefaults,
        haze_store::PublicModeSettings,
        admin_routes::AdminUserResp,
        admin_routes::CreateUserReq,
        admin_routes::UpdateUserReq,
        admin_routes::ResetPasswordReq,
        alerts_routes::AlertRuleResp,
        alerts_routes::AlertRuleReq,
        alerts_routes::AlertTargetDto,
        alerts_routes::AlertStateResp,
        alerts_routes::WebhookResp,
        alerts_routes::WebhookReq,
        alerts_routes::WebhookHeader,
        alerts_routes::WebhookTestResp,
        alerts_routes::WebhookDeleteConflictResp,
        haze_alert::types::Metric,
        haze_alert::types::Aggregation,
        haze_alert::types::Direction,
        haze_alert::types::Severity,
        haze_alert::types::TargetKind,
        replication_routes::InstanceInfoResp,
        replication_routes::PeerResp,
        replication_routes::CreatePeerReq,
        replication_routes::UpdatePeerReq,
        replication_routes::PeerTestResp,
        replication_routes::GroupPreviewResp,
        replication_routes::RuleResp,
        replication_routes::CreateRuleReq,
        replication_routes::UpdateRuleReq,
        replication_routes::InboundSlotResp,
        replication_routes::UpsertSlotReq,
        replication_routes::UpsertSlotResp,
        replication_routes::ManifestResp,
        replication_routes::ManifestGroup,
        replication_routes::ManifestHost,
        replication_routes::RangeResp,
        replication_routes::RangeSample,
        replication_routes::AckEntry,
    )),
    tags(
        (name = "probes", description = "Available probe types and their config schemas"),
        (name = "auth", description = "Password login + session cookie"),
        (name = "passkeys", description = "WebAuthn passkey registration + authentication"),
        (name = "user", description = "Self-service: password, passkeys, API tokens"),
        (name = "groups", description = "Host group tree (materialized path)"),
        (name = "hosts", description = "Hosts + time-series queries"),
        (name = "tree", description = "Combined group + host listing for the sidebar"),
        (name = "settings", description = "System-wide settings (admin only)"),
        (name = "admin", description = "Admin user management (admin only)"),
        (name = "alerts", description = "Alert rules, current states, and the webhook library"),
        (name = "replication", description = "Cross-instance time-series replication (admin-only)")
    )
)]
struct ApiDoc;

/// Endpoints callable without a session or token regardless of settings:
/// public-info pings, the login routes themselves.
const ALWAYS_ANONYMOUS: &[(&str, &str)] = &[
    ("/api/v1/server-info", "get"),
    ("/api/v1/probes", "get"),
    ("/api/v1/auth/login", "post"),
    ("/api/v1/auth/passkey/login/begin", "post"),
    ("/api/v1/auth/passkey/login/finish", "post"),
];

/// Endpoints anonymous-callable ONLY when public mode is enabled. Mirror
/// the handlers that take `ViewerAccess` so the spec's padlocks track the
/// runtime gate exactly.
const PUBLIC_MODE_ANONYMOUS: &[(&str, &str)] = &[
    ("/api/v1/tree", "get"),
    ("/api/v1/groups", "get"),
    ("/api/v1/groups/{uuid}", "get"),
    ("/api/v1/hosts", "get"),
    ("/api/v1/hosts/{uuid}", "get"),
    ("/api/v1/hosts/{uuid}/series", "get"),
    ("/api/v1/hosts/{uuid}/route-history", "get"),
    ("/api/v1/hosts/{uuid}/route-history/{id}", "get"),
    ("/api/v1/settings/storage", "get"),
    ("/api/v1/alerts/rules", "get"),
    ("/api/v1/alerts/rules/{uuid}", "get"),
    ("/api/v1/alerts/states", "get"),
];

/// Serve the generated `OpenAPI` document.
///
/// Two runtime rewrites on top of the compile-time `ApiDoc`:
///
/// 1. `servers` is injected from `state.cookie_path` so the spec stays
///    correct under `HAZE_BASE_URL` - the path entries are hardcoded
///    `/api/v1/...` and any consumer (Swagger UI's "Try it out", client
///    generators) needs the deployment base prefix.
///
/// 2. `security: []` is set on anonymous-callable endpoints so Swagger UI
///    doesn't render a padlock that misrepresents them as authenticated.
///    The conditional set tracks the live `public_mode.enabled` setting,
///    so the padlock disappears the moment an admin flips public mode on
///    and reappears when they flip it off.
///
/// Serializing to `serde_json::Value` and mutating that avoids coupling
/// to utoipa's internal path/operation struct layout, which has changed
/// across releases.
async fn openapi_json(State(state): State<AppState>) -> Json<serde_json::Value> {
    let spec = ApiDoc::openapi();
    let mut value = serde_json::to_value(&spec).expect("OpenApi spec must serialise");

    let server_url = if state.cookie_path.is_empty() {
        "/".to_string()
    } else {
        state.cookie_path.clone()
    };
    value["servers"] = serde_json::json!([{ "url": server_url }]);

    let public_mode_enabled = store_settings::public_mode_settings(&state.pool)
        .await
        .is_ok_and(|s| s.enabled);
    let mut anon: Vec<(&str, &str)> = ALWAYS_ANONYMOUS.to_vec();
    if public_mode_enabled {
        anon.extend_from_slice(PUBLIC_MODE_ANONYMOUS);
    }

    if let Some(paths) = value.get_mut("paths").and_then(|v| v.as_object_mut()) {
        for (path, method) in &anon {
            if let Some(op) = paths
                .get_mut(*path)
                .and_then(|p| p.as_object_mut())
                .and_then(|p| p.get_mut(*method))
                .and_then(|m| m.as_object_mut())
            {
                // Empty array = "no security requirement" in OpenAPI 3.x;
                // overrides the top-level global declaration for this op.
                op.insert("security".to_string(), serde_json::json!([]));
            }
        }
    }

    Json(value)
}

#[derive(Serialize, utoipa::ToSchema)]
struct ProbeDescriptor {
    kind: String,
    #[schema(value_type = Object, additional_properties = true)]
    config_schema: serde_json::Value,
}

#[derive(Serialize, utoipa::ToSchema)]
struct ServerInfo {
    /// Whether `WebAuthn` passkeys are configured (i.e. `HAZE_ORIGIN` was set).
    passkeys_enabled: bool,
    /// Whether anonymous browsing of the dashboard is enabled. When true,
    /// the frontend renders a trimmed read-only UI for visitors without a
    /// session and the read API endpoints accept anonymous calls.
    public_mode_enabled: bool,
    /// Server version (`CARGO_PKG_VERSION`).
    version: String,
    /// Stable per-instance UUID. Surfaced so the Settings page can show
    /// `My instance id: …` next to the Replication section without
    /// hitting the admin-only `/replication/instance-info` endpoint.
    instance_uuid: Uuid,
}

#[utoipa::path(
    get,
    path = "/api/v1/server-info",
    responses(
        (status = 200, body = ServerInfo, description = "Server feature flags and version. Anonymous-accessible.")
    ),
    tag = "probes"
)]
async fn server_info(State(state): State<AppState>) -> Json<ServerInfo> {
    // Reading the public-mode flag is intentionally not gated: the frontend
    // calls this before login to decide whether to render the trimmed
    // public layout, and `bool` reveals nothing sensitive on its own.
    let public_mode_enabled = store_settings::public_mode_settings(&state.pool)
        .await
        .is_ok_and(|s| s.enabled);
    Json(ServerInfo {
        passkeys_enabled: state.passkeys.is_some(),
        public_mode_enabled,
        version: env!("CARGO_PKG_VERSION").to_string(),
        instance_uuid: state.instance_uuid,
    })
}

#[utoipa::path(
    get,
    path = "/api/v1/probes",
    responses(
        (status = 200, body = Vec<ProbeDescriptor>, description = "Available probe types + their JSON-schema config")
    ),
    tag = "probes"
)]
async fn list_probes() -> Json<Vec<ProbeDescriptor>> {
    Json(
        ProbeKind::ALL
            .iter()
            .map(|k| ProbeDescriptor {
                kind: k.as_str().into(),
                config_schema: serde_json::from_str(k.config_schema())
                    .unwrap_or(serde_json::Value::Null),
            })
            .collect(),
    )
}
