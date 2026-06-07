mod ask;
mod classify;
mod deep;
mod describe;
mod doctor;
mod export;
mod fingerprint;
mod graph;
mod helpers;
mod index;
mod insights;
mod map;
mod mcp;
mod pack;
mod prune;
mod related;
mod report;
mod rm;
mod saved;
mod scan;
mod search;
mod serve;
mod status;
mod summarize;
mod update;
mod watch;
mod weight;
mod worker;

pub(crate) use ask::cmd_ask;
pub(crate) use classify::cmd_classify;
pub(crate) use deep::cmd_deep;
pub(crate) use describe::cmd_describe;
pub(crate) use doctor::cmd_doctor;
pub(crate) use export::cmd_export;
pub(crate) use fingerprint::cmd_fingerprint;
pub(crate) use graph::cmd_graph;
pub(crate) use index::cmd_index;
pub(crate) use insights::{
    cmd_insights_diff, cmd_insights_duplicates, cmd_insights_languages, cmd_insights_largest,
    cmd_insights_stale,
};
pub(crate) use map::cmd_map;
pub(crate) use mcp::cmd_mcp;
pub(crate) use pack::{
    cmd_pack_add, cmd_pack_create, cmd_pack_delete, cmd_pack_export, cmd_pack_list,
    cmd_pack_remove, cmd_pack_rename, cmd_pack_show,
};
pub(crate) use prune::cmd_prune;
pub(crate) use related::cmd_related;
pub(crate) use report::cmd_report;
pub(crate) use rm::cmd_rm;
pub(crate) use saved::{cmd_saved_add, cmd_saved_list, cmd_saved_rm, cmd_saved_run};
pub(crate) use scan::cmd_scan;
pub(crate) use search::cmd_search;
pub(crate) use serve::cmd_serve;
pub(crate) use status::cmd_status;
pub(crate) use summarize::cmd_summarize;
pub(crate) use update::cmd_update;
pub(crate) use watch::cmd_watch;
pub(crate) use weight::{
    cmd_weight_apply, cmd_weight_delete, cmd_weight_get, cmd_weight_list, cmd_weight_set,
    cmd_weight_suggest,
};
pub(crate) use worker::cmd_worker;
