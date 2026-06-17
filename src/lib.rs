mod bootstrap;
mod cascade;
mod commit;
mod config;
mod error;
mod inspect;
mod machine;
mod repo;
mod scope_graph;
mod status;
mod sync;

pub use crate::bootstrap::{init, InitReport};
pub use crate::commit::{
    commit_and_sync, continue_after_conflict, CommandOutcome, CommitOptions, CommitReport,
    CommitSelection, ContinueReport,
};
pub use crate::config::DotsyncPaths;
pub use crate::error::{DotsyncError, ErrorReport};
pub use crate::inspect::{
    diff_home, list_scope_tree, list_scopes, read_config_at_scope, read_scope_file, DiffReport,
    FileReport, ScopeInfo, ScopeListReport, TreeReport,
};
pub use crate::status::{status, ChangeStatus, FileChange, StatusReport};
pub use crate::sync::{sync, FileDrift, SyncOptions, SyncReport};
