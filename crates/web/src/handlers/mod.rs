//! HTTP request handlers, grouped by concern. The axum `Router` wiring in
//! `lib.rs` references these via `crate::handlers::*`; signatures are unchanged
//! from when they lived in `lib.rs`.

mod ask;
mod config;
mod fs;
mod jobs;
mod keys;
mod misc;
mod models;
mod providers;
mod queue;
mod stats;
mod summary;
mod telemetry;
mod tree;
mod ui;

pub(crate) use ask::api_ask;
pub(crate) use config::{
    api_config_get, api_config_passes, api_config_provider_set, api_config_resource_get,
    api_config_resource_set,
};
pub(crate) use fs::api_fs_ls;
pub(crate) use jobs::{
    api_job_deep, api_job_delete, api_job_estimate, api_job_get, api_job_index, api_job_scan,
    api_job_summarize, api_jobs_events, api_jobs_list,
};
pub(crate) use keys::{api_keys_get, api_keys_set};
pub(crate) use misc::{api_delete_entry, api_logs_tail, api_version};
pub(crate) use models::{
    api_models, api_models_catalog_refresh, api_models_installed, api_models_pull,
};
pub(crate) use providers::api_providers_status;
pub(crate) use queue::{api_queue_failed, api_queue_retry, api_queue_stats};
pub(crate) use stats::{api_map, api_roots, api_search, api_stats};
pub(crate) use summary::{api_summarize_enqueue, api_summary};
pub(crate) use telemetry::{api_telemetry, api_telemetry_stream};
pub(crate) use tree::api_tree;
pub(crate) use ui::{serve_ui, serve_ui_css, serve_ui_js};
