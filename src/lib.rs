//! Project Foil: speculative workspace branching for agent workloads.

#[cfg(not(target_os = "linux"))]
compile_error!("foil requires Linux (pidfd, signalfd, namespaces)");

pub mod agent;
pub mod branch;
pub mod cgroup;
pub mod doctor;
pub mod isolate;
pub mod limits;
pub mod pid1;
pub mod proxy;
pub mod strict;
pub mod supervisor;
