//! HTTP API. `axum` routers + `utoipa` `OpenAPI` generation.

use axum::{Json, Router, extract::State, middleware as axum_mw, response::Html, routing::get};
use haze_probe::ProbeKind;
use serde::Serialize;
use utoipa::{
    Modify, OpenApi,
    openapi::security::{ApiKey, ApiKeyValue, HttpAuthScheme, HttpBuilder, SecurityScheme},
};

mod admin_routes;
mod alerts_routes;
mod auth_routes;
mod error;
mod events_routes;
mod groups_routes;
mod hosts_routes;
mod middleware;
mod passkey_routes;
mod settings_routes;
mod state;
mod tree_routes;
mod user_routes;

pub use events_routes::ChangeKind;
pub use state::AppState;

pub fn api_router(state: AppState) -> Router {
    let v1 = v1_router(&state).with_state(state);

    Router::new()
        .nest("/v1", v1)
        .route("/openapi.json", get(openapi_json))
        .route("/docs", get(swagger_ui_html))
}

async fn swagger_ui_html() -> Html<&'static str> {
    Html(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <title>Haze API</title>
    <link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/swagger-ui-dist@5/swagger-ui.css">
    <style>body{margin:0;background:#fafbfc}</style>
</head>
<body>
    <div id="swagger-ui"></div>
    <script src="https://cdn.jsdelivr.net/npm/swagger-ui-dist@5/swagger-ui-bundle.js"></script>
    <script>
        window.onload = () => {
            window.ui = SwaggerUIBundle({
                url: 'openapi.json',
                dom_id: '#swagger-ui',
                deepLinking: true,
                persistAuthorization: true
            });
        };
    </script>
</body>
</html>
"#,
    )
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
        tree_routes::tree,
        settings_routes::get_storage,
        settings_routes::update_storage,
        settings_routes::get_workers,
        settings_routes::update_workers,
        settings_routes::get_alerting,
        settings_routes::update_alerting,
        settings_routes::get_host_defaults,
        settings_routes::update_host_defaults,
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
        haze_store::AlertingSettings,
        haze_store::HostDefaults,
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
        (name = "alerts", description = "Alert rules, current states, and the webhook library")
    )
)]
struct ApiDoc;

async fn openapi_json() -> Json<utoipa::openapi::OpenApi> {
    Json(ApiDoc::openapi())
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
    /// Server version (`CARGO_PKG_VERSION`).
    version: String,
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
    Json(ServerInfo {
        passkeys_enabled: state.passkeys.is_some(),
        version: env!("CARGO_PKG_VERSION").to_string(),
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
