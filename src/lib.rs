//! Project Foil: speculative workspace branching for agent workloads.
//!
//! Linux builds branches from overlayfs in user and mount namespaces and
//! the sandbox from namespaces and cgroups. macOS has none of those: there
//! branches are APFS clones and the sandbox is Seatbelt (`src/mac/`). The
//! modules below keep the same names on both, so the command line and
//! everything above it (fanout, the proxy, agent profiles) are shared.

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!("foil runs on Linux and macOS");

pub mod agent;
pub mod at;
pub mod change;
pub mod fanout;
pub mod gitignore;
pub mod proxy;
pub mod units;

#[cfg(target_os = "linux")]
pub mod branch;
#[cfg(target_os = "linux")]
pub mod cgroup;
#[cfg(target_os = "linux")]
pub mod container;
#[cfg(target_os = "linux")]
pub mod doctor;
#[cfg(target_os = "linux")]
pub mod isolate;
#[cfg(target_os = "linux")]
pub mod limits;
#[cfg(target_os = "linux")]
pub mod pid1;
#[cfg(target_os = "linux")]
pub mod strict;
#[cfg(target_os = "linux")]
pub mod supervisor;

#[cfg(target_os = "macos")]
#[path = "mac/branch.rs"]
pub mod branch;
#[cfg(target_os = "macos")]
#[path = "mac/doctor.rs"]
pub mod doctor;
#[cfg(target_os = "macos")]
#[path = "mac/isolate.rs"]
pub mod isolate;
#[cfg(target_os = "macos")]
#[path = "mac/limits.rs"]
pub mod limits;
#[cfg(target_os = "macos")]
#[path = "mac/procs.rs"]
pub mod procs;
#[cfg(target_os = "macos")]
#[path = "mac/supervisor.rs"]
pub mod supervisor;
#[cfg(target_os = "macos")]
#[path = "mac/vm.rs"]
pub mod vm;
