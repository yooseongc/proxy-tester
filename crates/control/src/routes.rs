use axum::{
    Router,
    extract::DefaultBodyLimit,
    routing::{get, post},
};
use tower_http::{
    cors::CorsLayer,
    services::{ServeDir, ServeFile},
    trace::TraceLayer,
};

use crate::{
    active_run, agents, apply_network_profile, archive_network_profile, diagnose_network,
    events_ws, export_run, generate_tls_certificate, health, list_artifacts, list_network_profiles,
    list_network_revisions, list_runs, list_runs_page, list_scenarios, network_audit, pause_run,
    plan_network_profile, preflight, reconcile_node, resume_run, run_detail, run_samples,
    run_summary_detail, save_network_profile, save_scenario, start_run, state::AppState, stop_run,
    teardown_network_profile, upload_artifact, validate_scenario,
};

pub(crate) fn build(state: AppState, static_dir: &str) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/agents", get(agents))
        .route(
            "/api/network/profiles",
            get(list_network_profiles).post(save_network_profile),
        )
        .route(
            "/api/network/profiles/{id}/plan",
            post(plan_network_profile),
        )
        .route(
            "/api/network/profiles/{id}/archive",
            post(archive_network_profile),
        )
        .route(
            "/api/network/operations/{id}/apply",
            post(apply_network_profile),
        )
        .route(
            "/api/network/revisions/{id}/teardown",
            post(teardown_network_profile),
        )
        .route("/api/network/audit", get(network_audit))
        .route("/api/network/diagnose", post(diagnose_network))
        .route("/api/network/nodes/{id}/reconcile", post(reconcile_node))
        .route("/api/network/revisions", get(list_network_revisions))
        .route("/api/scenarios", get(list_scenarios).post(save_scenario))
        .route("/api/scenarios/validate", post(validate_scenario))
        .route("/api/preflight", post(preflight))
        .route("/api/tls/certificates", post(generate_tls_certificate))
        .route("/api/artifacts", get(list_artifacts).post(upload_artifact))
        .route("/api/runs", get(list_runs).post(start_run))
        .route("/api/runs/page", get(list_runs_page))
        .route("/api/runs/active", get(active_run))
        .route("/api/runs/{id}", get(run_detail))
        .route("/api/runs/{id}/summary", get(run_summary_detail))
        .route("/api/runs/{id}/samples", get(run_samples))
        .route("/api/runs/{id}/export", get(export_run))
        .route("/api/runs/{id}/stop", post(stop_run))
        .route("/api/runs/{id}/pause", post(pause_run))
        .route("/api/runs/{id}/resume", post(resume_run))
        .route("/api/events/ws", get(events_ws))
        .fallback_service(
            ServeDir::new(static_dir)
                .not_found_service(ServeFile::new(format!("{static_dir}/index.html"))),
        )
        .layer(DefaultBodyLimit::max(513 * 1024 * 1024))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
