//! Config parser and transformation engine. Pure functions over an in-memory change set.
//! This crate does not call git, the network, or a model.

pub mod closure;
pub mod comments;
mod error;
pub mod gate;
mod globmatch;
mod receipt;
pub mod rust_cascade;
mod strip;
mod transform;
mod workflow;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub use error::ConfigError;
pub use receipt::{OriginRange, Receipt, ReceiptDrop, ReceiptLintAllow, ReceiptVerify};
pub use strip::{strip_references, Stripped};
pub use transform::{
    apply, apply_window, finish_squash, first_line, held_blobs, squash_subjects, Blob, Commit,
    Drop, EditMemo, MessageRewriter, Rewritten, StripEvent, StripMemo, StubRewriter,
    TransformResult, WindowState,
};
pub use workflow::{
    parse_workflow, Auth, Authoring, Destination, Identity, LintAllow, Mode, Origin, PathFilter,
    Scope, Transformation, Verify, Workflow,
};

#[cfg(test)]
mod tests {
    #[test]
    fn the_crate_names_its_version() {
        assert_eq!(super::VERSION, "0.0.1");
    }
}
