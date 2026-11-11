//! Web server that merges imsg-extract snapshots into a searchable index and
//! hosts them behind password authentication.

pub mod auth;
pub mod index;
pub mod media;
pub mod queries;
pub mod render;
pub mod web;
