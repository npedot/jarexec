//! jarexec as a library: the building blocks of the `jarexec` launcher, for tools that
//! manage several applications (e.g. installing a suite into one shared cache).
//!
//! The usual flow is [`lockfile::Lockfile::load`], then [`java::JavaResolver::resolve`]
//! and [`fetch::Fetcher::fetch_all`] to fill a cache, and [`launch::prepare`] plus
//! [`launch::exec`] to start the application. [`clean::cache_lock`] guards a cache
//! shared between processes.

pub mod clean;
pub mod deps_list;
pub mod fetch;
pub mod hash;
pub mod http;
pub mod jar;
pub mod java;
pub mod launch;
pub mod lock;
pub mod lockfile;
pub mod paths;
pub mod platform;
#[cfg(test)]
mod test_util;
