//! Integration tests of rconn-signaling, built as one test program (one link instead
//! of one per file). Each file below is a module; run one with e.g.
//! `cargo nextest run -p rconn-signaling -E 'test(portal_auth::)'`.

mod broker;
mod fleet_auth;
mod oidc;
mod portal_audit_db;
mod portal_auth;
mod portal_computers;
mod portal_db;
mod portal_people;
mod portal_policies;
mod portal_sso;
mod portal_support;
mod portal_tenancy;
mod public;
mod reload;
