mod bootstrap;
mod cascade;
mod commit;
mod config;
mod drift;
mod error;
mod inspect;
mod machine;
mod repo;
mod scope_graph;
mod status;
mod sync;

pub use crate::bootstrap::{init, InitReport};
pub use crate::commit::{
    abort_paused_cascade, commit_and_sync, continue_after_conflict, AbortReport, CommitOptions,
    CommitReport, ContinueReport,
};
pub use crate::config::DotsyncPaths;
pub use crate::drift::FileState;
pub use crate::error::{
    CommitPathProblem, DotsyncError, ErrorReport, RefusedCommitPath, RejectedCommitPath,
};
pub use crate::inspect::{
    diff_home, list_scope_tree, list_scopes, read_scope_file, DiffReport, FileReport, ScopeInfo,
    ScopeListReport, TreeReport,
};
pub use crate::repo::PushReport;
pub use crate::status::{status, FileChange, StatusReport};
pub use crate::sync::{sync, FileDrift, ForceScope, SyncCommandReport, SyncReport};
