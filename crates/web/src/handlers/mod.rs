//! HTTP request handlers, grouped by concern. The axum `Router` wiring in
//! `lib.rs` references these via `crate::handlers::*`; signatures are unchanged
//! from when they lived in `lib.rs`.

mod ask;
mod classify;
mod config;
mod export;
mod fs;
mod graph;
mod insights_handler;
mod jobs;
mod keys;
mod misc;
mod models;
mod packs;
mod providers;
mod queue;
mod review;
mod saved;
mod stats;
mod summary;
mod telemetry;
mod tree;
mod ui;
mod update;
mod watch;
mod weights;

pub(crate) use ask::{api_ask, api_ask_stream};
pub(crate) use classify::{
    api_classifications_confirm, api_classifications_ignore, api_classifications_list,
    api_classifications_reset,
};
pub(crate) use config::{
    api_config_features_get, api_config_features_set, api_config_get, api_config_passes,
    api_config_provider_set, api_config_resource_get, api_config_resource_set,
};
pub(crate) use export::api_export;
pub(crate) use fs::api_fs_ls;
pub(crate) use graph::api_graph;
pub(crate) use insights_handler::{
    api_insights_diff, api_insights_duplicates, api_insights_stale, api_review_dismiss_evidence,
};
pub(crate) use jobs::{
    api_job_deep, api_job_delete, api_job_estimate, api_job_get, api_job_index, api_job_scan,
    api_job_summarize, api_jobs_events, api_jobs_list,
};
pub(crate) use keys::{api_keys_get, api_keys_set};
pub(crate) use misc::{api_delete_entry, api_logs_tail, api_version};
pub(crate) use models::{
    api_models, api_models_catalog_refresh, api_models_installed, api_models_pull,
};
pub(crate) use packs::{
    api_packs_create, api_packs_delete, api_packs_export, api_packs_list, api_packs_paths_add,
    api_packs_paths_get, api_packs_paths_remove, api_packs_search, api_packs_suggest,
};
pub(crate) use providers::api_providers_status;
pub(crate) use queue::{api_queue_failed, api_queue_retry, api_queue_stats};
pub(crate) use review::{
    api_review_answer, api_review_answer_batch, api_review_count, api_review_dismiss,
    api_review_history, api_review_list, api_review_revert,
};
pub(crate) use saved::{api_saved_delete, api_saved_list, api_saved_set};
pub(crate) use stats::{api_map, api_map_treemap, api_roots, api_search, api_stats};
pub(crate) use summary::{api_summarize_enqueue, api_summary};
pub(crate) use telemetry::{api_engine_release, api_telemetry, api_telemetry_stream};
pub(crate) use tree::api_tree;
pub(crate) use ui::{serve_ui, serve_ui_css, serve_ui_js};
pub(crate) use update::{api_update_apply, api_update_check};
pub(crate) use watch::{api_watch_start, api_watch_status, api_watch_stop};
pub(crate) use weights::{
    api_weights_delete, api_weights_list, api_weights_set, api_weights_suggest,
};
