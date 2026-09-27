mod bootstrap;
mod commit;
mod converge;
mod drift;
mod error;
mod fleet;
mod home;
mod inspect;
mod machine;
mod paths;
mod pause;
mod place;
mod repo;
mod scope_graph;
mod selection;
mod session;
mod status;
mod sync;
mod working_copy;

pub use crate::bootstrap::{
    create_scope, delete_scope, init, CreatedScope, DeletedScope, InitReport,
};
pub use crate::commit::{commit_and_sync, CommitOptions, CommitReport, RecordedCommit};
pub use crate::drift::FileState;
pub use crate::error::{
    CommitPathProblem, ConflictRole, ConflictedFile, ConflictedVersion, DotsyncError, Explanation,
    RefusedCommitPath, RejectedCommitPath, SkipReason, SkippedCommitPath, Teaching,
};
pub use crate::fleet::{
    Change, EntryKind, FileRow, MachineEffect, PathChange, ScopeInfo, Standing,
};
pub use crate::inspect::{
    compare, diff_home, files, scopes, show, CompareReport, DiffReport, FilesQuery, FilesReport,
    ScopeDifference, ScopesReport, ShowReport,
};
pub use crate::paths::DotsyncPaths;
pub use crate::pause::{
    abort_paused_cascade, continue_after_conflict, AbortReport, ContinueReport, Resumed,
};
pub use crate::place::{place, CarriedOut, PlacementOptions, PlacementReport, Planned};
pub use crate::repo::PushReport;
pub use crate::session::{Run, UnreachableRemote};
pub use crate::status::{status, FileChange, MachineState, PausedCascade, StatusReport};
pub use crate::sync::{discard, sync, FileDrift, SyncCommandReport, SyncReport};
