//! The mode vocabulary: the autonomy levels a session runs under.

use std::sync::Arc;
use std::sync::Mutex;

/// The session's autonomy level, selected by the configuration's
/// `permission_mode` key, the `--permission-mode` flag, or a live
/// `/mode` move.
///
/// It is (de)serialized as its `snake_case` name, and the same string is
/// what the display shows, so what the user configured and what the
/// session reports never drift apart. The variants are ordered from the
/// most autonomous to the most conservative.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PermissionMode {
    /// Never prompts.
    ///
    /// Lets every side-effecting action run without confirmation. The most
    /// autonomous mode.
    #[default]
    Auto,

    /// Only looks: tools and shell commands that never write or mutate.
    ///
    /// Reads run free — file tools, and shell commands the reader gate
    /// proves only look (a pipeline runs only when **every** segment
    /// reads) — and the session's own furniture (the todo list, the
    /// question tool) with them. Everything that writes, executes beyond
    /// looking, or reaches the network is blocked without asking. The
    /// line is absolute: no rule loosens it.
    ReadOnly,

    /// The user decides: every uncovered side-effecting call prompts.
    ///
    /// Reads run free, and the session's own furniture with them; the
    /// user's answers record into the two remembered lists — always
    /// allow and always deny — so a decision is made once per kind of
    /// work, and everything not covered by a rule (or a preset) prompts.
    Interactive,
}

/// The permission mode as a shared, live cell.
///
/// The policy reads the mode from this cell on every evaluation rather
/// than freezing it at construction, so a host can move the mode while a
/// run is in flight — `/mode` mid-run — and the very next dispatch runs
/// under it. A plain alias: the sharing is the whole point.
pub type SharedPermissionMode = Arc<Mutex<PermissionMode>>;

impl PermissionMode {
    /// The mode's config spelling.
    ///
    /// The `snake_case` name serde reads and writes — the same string
    /// the TUI's status bar shows, so what the user configured and
    /// what the display names never drift apart.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::ReadOnly => "read_only",
            Self::Interactive => "interactive",
        }
    }

    /// The mode a config spelling names, accepting the retired ones.
    ///
    /// The current spellings map directly. The two retired spellings map
    /// to their replacements — `plan` was renamed `read_only` with
    /// identical semantics, and `accept_edits` dissolved into
    /// `interactive` (its auto-applied edits become an explicit allow
    /// rule a preset or config grants) — each with a warning naming the
    /// migration, so a stale config announces its update instead of
    /// silently changing meaning. `None` for anything else.
    #[must_use]
    pub fn from_wire(spelling: &str) -> Option<Self> {
        match spelling {
            "auto" => Some(Self::Auto),
            "read_only" => Some(Self::ReadOnly),
            "interactive" => Some(Self::Interactive),
            "plan" => {
                tracing::warn!(
                    "permission_mode = \"plan\" is now \"read_only\" (same \
                     behavior, new name) — update the spelling"
                );
                Some(Self::ReadOnly)
            }
            "accept_edits" => {
                tracing::warn!(
                    "permission_mode = \"accept_edits\" is now \"interactive\": \
                     file edits prompt unless a rule (or preset) allows them — \
                     add [[permissions.rules]] outcome = \"allow\" tool = \"Write\" \
                     to keep the old behavior"
                );
                Some(Self::Interactive)
            }
            _ => None,
        }
    }
}

impl serde::Serialize for PermissionMode {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> serde::Deserialize<'de> for PermissionMode {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let spelling = String::deserialize(deserializer)?;
        Self::from_wire(&spelling).ok_or_else(|| {
            serde::de::Error::unknown_variant(&spelling, &["auto", "read_only", "interactive"])
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::missing_panics_doc)]
mod tests {
    use super::*;

    #[test]
    fn spellings_match_the_config_wire_names() {
        assert_eq!(PermissionMode::Auto.as_str(), "auto");
        assert_eq!(PermissionMode::ReadOnly.as_str(), "read_only");
        assert_eq!(PermissionMode::Interactive.as_str(), "interactive");
        assert_eq!(
            serde_json::from_str::<PermissionMode>("\"interactive\"").unwrap(),
            PermissionMode::Interactive,
            "serde reads the same spelling as_str writes"
        );
    }

    #[test]
    fn the_retired_spellings_migrate_with_a_mapping() {
        // The two removed modes keep loading: plan was a pure rename,
        // accept_edits dissolved into interactive, and both migrations
        // warn so a stale config announces itself.
        assert_eq!(
            PermissionMode::from_wire("plan"),
            Some(PermissionMode::ReadOnly),
            "plan was renamed read_only with identical semantics"
        );
        assert_eq!(
            PermissionMode::from_wire("accept_edits"),
            Some(PermissionMode::Interactive),
            "accept_edits dissolved into interactive"
        );
        assert_eq!(PermissionMode::from_wire("yolo"), None);
    }
}
