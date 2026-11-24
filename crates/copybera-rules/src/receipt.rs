use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::transform::Drop;

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct OriginRange {
    pub since: Option<String>,
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub resolved_since: Option<String>,
    pub resolved_ref: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ReceiptLintAllow {
    pub paths: Vec<String>,
    pub patterns: Vec<usize>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ReceiptDrop {
    pub commit: String,
    pub reason: String,
}

/// One `[verify]` command's outcome.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ReceiptVerify {
    pub command: Vec<String>,
    pub ok: bool,
    /// The exit code; absent when the command could not start or was killed by a signal.
    pub code: Option<i32>,
    pub seconds: f64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Receipt {
    pub workflow: String,
    pub dry_run: bool,
    pub commits_read: u64,
    pub commits_written: u64,
    pub files_kept: u64,
    pub files_stripped: u64,
    pub commits_dropped: u64,
    pub origin_range: OriginRange,
    /// The last origin commit this run walked: the ref for a whole-range run, the last walked
    /// commit for a run cut by `--max-commits`.
    pub last_rev: Option<String>,
    /// True when `--max-commits` cut the walk short of the ref. A receipt written before the field existed has none and
    /// reads as false.
    #[serde(default)]
    pub bounded: bool,
    pub would_move: Vec<String>,
    pub drops: Vec<ReceiptDrop>,
    /// Origin sha → destination sha. Dry-run omits destination shas (`null`).
    pub origin_to_destination: BTreeMap<String, Option<String>>,
    /// Absolute destination path for receipt-directed tools.
    #[serde(default)]
    pub destination: Option<String>,
    /// Destination branch this run pushed.
    #[serde(default)]
    pub push_branch: Option<String>,
    /// The run's wall time in seconds, origin walk to receipt (`copybera status` shows it).
    #[serde(default)]
    pub seconds: Option<f64>,
    /// `stubbed` when the workflow names the message hook and this run wrote the fallback's subjects (the hook is
    /// reserved and not wired yet, so a run meant to use it is not ready); absent when the
    /// workflow names no hook.
    #[serde(default)]
    pub model_hook: Option<String>,
    /// The private-patterns file the workflow's `[lint] private` names, absolute, so `lint --from-receipt` reads it.
    #[serde(default)]
    pub private_patterns: Option<String>,
    /// The workflow's private-pattern path allowances, so receipt-directed lint has the same scope.
    #[serde(default)]
    pub lint_allow: Vec<ReceiptLintAllow>,
    /// The reference patterns the workflow's lint refuses (`[lint] ids`, else its `strip_references` patterns), so
    /// receipt-directed lint refuses the same ids.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub lint_ids: Vec<String>,
    /// The private scrub file the workflow's `[scrub] private` names, absolute.
    #[serde(default)]
    pub private_scrub: Option<String>,
    /// Number of active private scrub rules read for this run.
    #[serde(default)]
    pub private_scrub_rule_count: Option<u64>,
    /// Events per transformation label (`paths.origin`, `transformations[3].move_paths`, ...), summed over the walked
    /// commits.
    #[serde(default)]
    pub transform_events: BTreeMap<String, u64>,
    /// Distinct origin paths a rename or move took to a new name.
    #[serde(default)]
    pub paths_moved: u64,
    /// Distinct origin paths a move left in place because the new name was taken.
    #[serde(default)]
    pub paths_stayed: u64,
    /// `[cascade] rust`'s mode, when the workflow sets it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cascade_rust: Option<String>,
    /// `[verify]`'s commands in order, as far as they ran.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verify: Vec<ReceiptVerify>,
}

impl Receipt {
    pub fn to_json(&self) -> serde_json::Result<String> {
        serde_json::to_string_pretty(self)
    }

    pub fn from_drops(drops: &[Drop]) -> Vec<ReceiptDrop> {
        drops
            .iter()
            .map(|d| ReceiptDrop {
                commit: d.commit.clone(),
                reason: d.reason.clone(),
            })
            .collect()
    }
}
