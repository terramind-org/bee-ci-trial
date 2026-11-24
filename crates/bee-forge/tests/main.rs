//! The integration tests of `bee-forge`, built as one test program: each file under
//! `tests/` is a module here (same file, same test names), so the crate's code and its
//! dependencies are linked once instead of once per file. Run one file's tests with
//! `cargo test -p bee-forge --test integration <file_stem>::`.

#[macro_use]
mod common;
// Shared by the enterprise_* files (it exports `on_both_backends!`, so it can exist once).
#[macro_use]
#[path = "enterprise/support.rs"]
mod enterprise_support;
// Helpers shared by several test files, compiled once.
#[path = "enterprise/billing_models_backends.rs"]
mod billing_models_backends;
#[path = "bisect_pg_race.rs"]
mod bisect_pg_race;
#[path = "briefs_support.rs"]
mod briefs_support;
#[path = "packages/support.rs"]
mod packages_support;
mod security_support;

#[path = "adopt.rs"]
mod adopt;
#[path = "agents.rs"]
mod agents;
#[path = "audit.rs"]
mod audit;
#[path = "briefs_brief.rs"]
mod briefs_brief;
#[path = "briefs_evidence.rs"]
mod briefs_evidence;
#[path = "changes.rs"]
mod changes;
#[path = "ci.rs"]
mod ci;
#[path = "ci_followups.rs"]
mod ci_followups;
#[path = "community_backing.rs"]
mod community_backing;
#[path = "community_discussions.rs"]
mod community_discussions;
#[path = "community_notifications.rs"]
mod community_notifications;
#[path = "community_releases.rs"]
mod community_releases;
#[path = "community_sponsors_insights.rs"]
mod community_sponsors_insights;
#[path = "community_stores.rs"]
mod community_stores;
#[path = "community_support.rs"]
mod community_support;
#[path = "community_users.rs"]
mod community_users;
#[path = "community_wiki_gists.rs"]
mod community_wiki_gists;
#[path = "coverage_4_16.rs"]
mod coverage_4_16;
#[path = "coverage_4_9.rs"]
mod coverage_4_9;
#[path = "enterprise_apps.rs"]
mod enterprise_apps;
#[path = "enterprise_billing.rs"]
mod enterprise_billing;
#[path = "enterprise_integration.rs"]
mod enterprise_integration;
#[path = "enterprise_models.rs"]
mod enterprise_models;
#[path = "enterprise_moderation.rs"]
mod enterprise_moderation;
#[path = "enterprise_network.rs"]
mod enterprise_network;
#[path = "enterprise_owners_pg_race.rs"]
mod enterprise_owners_pg_race;
#[path = "enterprise_scim.rs"]
mod enterprise_scim;
#[path = "enterprise_sso.rs"]
mod enterprise_sso;
#[path = "enterprise_teams.rs"]
mod enterprise_teams;
#[path = "events.rs"]
mod events;
#[path = "forks.rs"]
mod forks;
#[path = "idempotent_runs_pg.rs"]
mod idempotent_runs_pg;
#[path = "ip_ranges_pg_race.rs"]
mod ip_ranges_pg_race;
#[path = "isolation.rs"]
mod isolation;
#[path = "landed_combinations.rs"]
mod landed_combinations;
#[path = "landing_fence_pg_race.rs"]
mod landing_fence_pg_race;
#[path = "leases.rs"]
mod leases;
#[path = "local_runner_limits.rs"]
mod local_runner_limits;
#[path = "owners_pg_race.rs"]
mod owners_pg_race;
#[path = "packages.rs"]
mod packages;
#[path = "pages.rs"]
mod pages;
#[path = "posts.rs"]
mod posts;
#[path = "release_assets_pg_race.rs"]
mod release_assets_pg_race;
#[path = "schedule_pg_race.rs"]
mod schedule_pg_race;
#[path = "security_advisories.rs"]
mod security_advisories;
#[path = "security_deps.rs"]
mod security_deps;
#[path = "security_scanning.rs"]
mod security_scanning;
#[path = "security_scenarios.rs"]
mod security_scenarios;
#[path = "security_service.rs"]
mod security_service;
#[path = "security_store.rs"]
mod security_store;
#[path = "service.rs"]
mod service;
#[path = "signature_verify.rs"]
mod signature_verify;
#[path = "signing_keys_pg_race.rs"]
mod signing_keys_pg_race;
#[path = "store_conformance.rs"]
mod store_conformance;
#[path = "stream_reauth.rs"]
mod stream_reauth;
#[path = "team_invite_pg_race.rs"]
mod team_invite_pg_race;
#[path = "verified_email_pg_race.rs"]
mod verified_email_pg_race;
#[path = "webhook_dispatch_pg.rs"]
mod webhook_dispatch_pg;
#[path = "webhooks.rs"]
mod webhooks;
#[path = "work_service.rs"]
mod work_service;
#[path = "work_store.rs"]
mod work_store;
