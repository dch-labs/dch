//! Workspace crate-dependency boundary enforcement.
//!
//! Parses `cargo metadata` output and verifies each dch crate's dependency set
//! matches its allowed shape, so an accidental cross-couple (e.g. `dch-tui`
//! growing a `dch-tools` dep) is caught at CI time rather than by code review.
//!
//! The core [`Checker`] is pure over a slice of [`Package`]s, so unit tests
//! exercise each rule with synthetic inputs without shelling out to `cargo`.

use std::process::Command;
use std::process::ExitCode;

/// One package as `cargo metadata` reports it, narrowed to what the rules read.
///
/// The metadata document carries far more per package than the boundary
/// needs; name, runtime dependency names, and the two facts saying where
/// the package came from are enough, which lets [`Checker`] stay pure
/// over a slice of these rather than over the raw document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    /// The crate name, spelled as its manifest spells it.
    ///
    /// The rules address packages by name alone. Should a resolved graph
    /// carry two versions of one crate, [`find`] inspects the first — the
    /// rules read declared edges, which any version of a crate states the
    /// same way.
    pub name: String,
    /// The runtime dependency names, workspace crates and externals alike.
    ///
    /// Dev- and build-dependencies are dropped before this view is built:
    /// they are not edges a shipped binary carries, so no rule may count
    /// them as a coupling.
    pub deps: Vec<String>,

    /// Where the crate resolves from, as `cargo metadata` reports it.
    ///
    /// `Some` for anything fetched — `"registry+…"` or `"git+…"`.
    /// `None` marks a crate read off the local filesystem, which a
    /// clean checkout has no way to obtain.
    pub source: Option<String>,

    /// Whether the crate belongs to this workspace.
    ///
    /// Members are read from disk by design; a non-member carrying no
    /// [`Package::source`] is a foreign path dependency.
    pub workspace_member: bool,
}

/// One boundary-rule violation, attributed to the crate that caused it.
///
/// Rules accumulate rather than fail fast, so a single run reports every
/// boundary problem in the manifest at once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// The crate whose own dependency list tripped the rule.
    ///
    /// Names where the fix belongs; [`Violation::detail`] names the edge
    /// that is wrong about it.
    pub crate_name: String,
    /// What the rule required and what the manifest actually declares.
    ///
    /// Printed verbatim by `check-boundary`, so a developer sees the
    /// expected shape beside the found one instead of a bare refusal.
    pub detail: String,
}

/// Whether `dep` names a crate of this workspace (`dch`, `dch-config`, …).
///
/// The naming convention is the only discriminator needed: every crate
/// published from this repository carries the prefix, and nothing fetched
/// from outside it does.
fn is_dch_crate(dep: &str) -> bool {
    dep == "dch" || dep.starts_with("dch-")
}

/// The crates the declared local bridge may redirect to a sibling
/// checkout, and no others.
///
/// Rule 5's carve-out names this family explicitly so a bridge can
/// never sanction a path dependency that is not the loopctl git
/// dependency it exists to redirect; a new foreign path dep fails the
/// rule bridged or not.
fn is_bridge_family(name: &str) -> bool {
    name == "loopctl" || name == "loopctl-derive"
}

/// The git URL the bridge redirects.
///
/// The exact dependency URL `Cargo.toml` names; the config's
/// `[patch]` table is keyed by it, so the URL — not a formatted
/// header string — is what the parsed lookup keys on.
const LOOPCTL_GIT_URL: &str = "https://github.com/dch-labs/loopctl.git";

/// Whether config text declares the local bridge.
///
/// Declaration means a live TOML `[patch]` table keyed by
/// [`LOOPCTL_GIT_URL`] — parsed, not searched for: a commented-out
/// header or the header's text carried inside a string value is not a
/// table, and only a table Cargo would actually honor may relax the
/// foreign-path rule. Unparseable text declares nothing (Cargo would
/// refuse it too). Pure over the text so the rule is pinnable without
/// owning a working directory or a config file.
fn bridge_declared_in(config: &str) -> bool {
    config.parse::<toml::Value>().is_ok_and(|value| {
        value
            .get("patch")
            .is_some_and(|patch| patch.get(LOOPCTL_GIT_URL).is_some())
    })
}

/// Whether the workspace's `.cargo/config.toml` declares the local
/// bridge.
///
/// Reads `path` — the caller hands the workspace root's config, from
/// `cargo metadata`'s own answer, so a check launched from a member
/// directory still reads the workspace's file rather than the
/// member's — and answers through [`bridge_declared_in`]. An
/// unreadable or absent file declares nothing. Git-ignored by
/// convention, so a clean checkout — CI included — never declares
/// one.
fn local_bridge_declared(config_path: &std::path::Path) -> bool {
    match std::fs::read_to_string(config_path) {
        Ok(config) => bridge_declared_in(&config),
        Err(_) => false,
    }
}

/// The `dch-*` subset of a package's declared dependencies.
///
/// Sorted and de-duplicated so a rule can compare the result against an
/// expected set without caring about declaration order or repeats.
fn dch_deps_of(pkg: &Package) -> Vec<String> {
    let mut v: Vec<String> = pkg
        .deps
        .iter()
        .filter(|d| is_dch_crate(d))
        .cloned()
        .collect();
    v.sort();
    v.dedup();
    v
}

/// The first package in `pkgs` carrying `name`, if any.
///
/// A rule that cannot find its package reports it as absent from the
/// metadata rather than passing quietly, so a renamed crate fails loudly.
fn find<'a>(pkgs: &'a [Package], name: &str) -> Option<&'a Package> {
    pkgs.iter().find(|p| p.name == name)
}

/// The boundary checker, pure over a slice of packages.
///
/// Holding no I/O lets every rule be driven with synthetic packages in
/// the unit tests, so a rule's verdict is pinned without a misbehaving
/// manifest or a `cargo metadata` invocation.
pub struct Checker<'a> {
    pkgs: &'a [Package],
}

impl<'a> Checker<'a> {
    /// A checker borrowing the packages it will inspect.
    ///
    /// The slice is normally the whole resolved graph: the rules select
    /// from it by name rather than the caller pre-filtering it.
    pub fn new(pkgs: &'a [Package]) -> Self {
        Self { pkgs }
    }

    /// Run all five rules, collecting every violation.
    ///
    /// The rules are independent, so one pass reports every boundary
    /// problem in the manifest rather than stopping at the first; an
    /// empty return is the verdict `check-boundary` prints as a pass.
    /// Reads the graph with no local bridge declared — the shape a clean
    /// checkout resolves.
    pub fn check(&self) -> Vec<Violation> {
        self.check_with_local_bridge(false)
    }

    /// Run the rules under a declared local bridge, which sanctions
    /// exactly the loopctl family resolving by path.
    ///
    /// The bridge is the documented developer workflow for exercising
    /// uncommitted sibling `loopctl` work through this workspace: an
    /// uncommitted `.cargo/config.toml` `[patch]` redirects the git
    /// dependency to the sibling checkout. Under that declaration rule 5
    /// spares path-resolved `loopctl`/`loopctl-derive` — every other
    /// foreign path dependency still violates, so the carve-out cannot
    /// launder a new one — and `run_check` prints a notice for each
    /// spared crate so a bridged run never reads as a clean-graph pass.
    pub fn check_with_local_bridge(&self, local_bridge: bool) -> Vec<Violation> {
        let mut out = Vec::new();
        out.extend(self.rule_dch_tools_loopctl_only());
        out.extend(self.rule_dch_tui_dch_config_only());
        out.extend(self.rule_loopctl_no_dch());
        out.extend(self.rule_exact_dch_dep_sets());
        out.extend(self.rule_no_foreign_path_deps(local_bridge));
        out
    }

    /// Rule 1: `dch-tools` depends on `loopctl` and on no `dch-*` crate.
    ///
    /// The tool layer is the crate meant to be reusable beyond this
    /// product, so it may not reach back into configuration or the loop
    /// wiring — any `dch-*` edge there turns a library-shaped crate into a
    /// component of one binary.
    fn rule_dch_tools_loopctl_only(&self) -> Vec<Violation> {
        let Some(pkg) = find(self.pkgs, "dch-tools") else {
            return vec![Violation {
                crate_name: "dch-tools".into(),
                detail: "package not present in cargo metadata".into(),
            }];
        };
        let dch_deps = dch_deps_of(pkg);
        if dch_deps.is_empty() {
            return vec![];
        }
        vec![Violation {
            crate_name: "dch-tools".into(),
            detail: format!(
                "must depend only on loopctl, but declares dch-* dep(s): {}",
                dch_deps.join(", ")
            ),
        }]
    }

    /// Rule 2: `dch-tui`'s `dch-*` dependencies are exactly `dch-config`.
    ///
    /// The display layer reads configuration and nothing else from inside
    /// the workspace — neither the tool implementations nor the agent
    /// loop — which is what keeps a transcript renderable on its own.
    fn rule_dch_tui_dch_config_only(&self) -> Vec<Violation> {
        let Some(pkg) = find(self.pkgs, "dch-tui") else {
            return vec![Violation {
                crate_name: "dch-tui".into(),
                detail: "package not present in cargo metadata".into(),
            }];
        };
        let actual = dch_deps_of(pkg);
        let expected = vec!["dch-config".to_string()];
        if actual == expected {
            return vec![];
        }
        vec![Violation {
            crate_name: "dch-tui".into(),
            detail: format!(
                "dch-* deps must be exactly {{dch-config}}, but are {{{}}}",
                actual.join(", ")
            ),
        }]
    }

    /// Rule 3: `loopctl` declares no dependency on any `dch-*` crate.
    ///
    /// The dependency between the framework and this product runs one
    /// way; an edge in the other direction would make the framework
    /// unbuildable without the product sitting beside it.
    fn rule_loopctl_no_dch(&self) -> Vec<Violation> {
        let Some(pkg) = find(self.pkgs, "loopctl") else {
            // `cargo metadata` walks the resolved graph, so loopctl shows up
            // whatever its source. Absence is a separate problem.
            return vec![Violation {
                crate_name: "loopctl".into(),
                detail: "package not present in cargo metadata (one-way rule P1 unverifiable)"
                    .into(),
            }];
        };
        let dch_deps = dch_deps_of(pkg);
        if dch_deps.is_empty() {
            return vec![];
        }
        vec![Violation {
            crate_name: "loopctl".into(),
            detail: format!(
                "must depend on no dch-* crate (one-way P1), but declares: {}",
                dch_deps.join(", ")
            ),
        }]
    }

    /// Rule 4: the exact `dch-*` dependency sets of `dch-loop` and `dch`.
    ///
    /// These two are the composition points, so their edges are fixed
    /// rather than merely constrained — configuration and tools beneath
    /// the loop, the three assembled crates beneath the binary. Anything
    /// else violates this rule even where rules 1 to 3 would allow it.
    fn rule_exact_dch_dep_sets(&self) -> Vec<Violation> {
        let mut out = Vec::new();
        let dch_loop_expected = vec!["dch-config".to_string(), "dch-tools".to_string()];
        if let Some(pkg) = find(self.pkgs, "dch-loop") {
            let actual = dch_deps_of(pkg);
            if actual != dch_loop_expected {
                out.push(Violation {
                    crate_name: "dch-loop".into(),
                    detail: format!(
                        "dch-* deps must be exactly {{dch-tools, dch-config}}, but are {{{}}}",
                        actual.join(", ")
                    ),
                });
            }
        } else {
            out.push(Violation {
                crate_name: "dch-loop".into(),
                detail: "package not present in cargo metadata".into(),
            });
        }

        let dch_binary_expected = vec![
            "dch-config".to_string(),
            "dch-loop".to_string(),
            "dch-tui".to_string(),
        ];
        if let Some(pkg) = find(self.pkgs, "dch") {
            let actual = dch_deps_of(pkg);
            if actual != dch_binary_expected {
                out.push(Violation {
                    crate_name: "dch".into(),
                    detail: format!(
                        "dch-* deps must be exactly {{dch-loop, dch-tui, dch-config}}, but are {{{}}}",
                        actual.join(", ")
                    ),
                });
            }
        } else {
            out.push(Violation {
                crate_name: "dch".into(),
                detail: "package not present in cargo metadata".into(),
            });
        }
        out
    }

    /// Rule 5: every package resolves from a fetched source or is a
    /// member of this workspace.
    ///
    /// A dependency declared by path outside the workspace builds only
    /// where the sibling checkout happens to sit: `Cargo.lock` records
    /// neither a source nor a checksum for it, so a clean checkout, a
    /// release build, and a container image that copies just the
    /// workspace all fail to resolve it, and the lockfile stops being a
    /// reproducible record of what was built. The sole exception is the
    /// declared local bridge
    /// ([`check_with_local_bridge`](Checker::check_with_local_bridge)):
    /// when one is active, path-resolved `loopctl`/`loopctl-derive`
    /// are the sanctioned sibling-patch workflow and pass — nothing
    /// else ever does.
    fn rule_no_foreign_path_deps(&self, local_bridge: bool) -> Vec<Violation> {
        self.pkgs
            .iter()
            .filter(|pkg| !pkg.workspace_member && pkg.source.is_none())
            .filter(|pkg| !(local_bridge && is_bridge_family(&pkg.name)))
            .map(|pkg| Violation {
                crate_name: pkg.name.clone(),
                detail: "resolves by path from outside the workspace; a clean \
                         checkout cannot fetch it"
                    .to_string(),
            })
            .collect()
    }
}

/// The part of the `cargo metadata` document this check reads.
///
/// Declaring only these fields keeps an unrelated metadata format change
/// harmless; the two listed are the ones the rules cannot be derived
/// without.
#[derive(Debug, serde::Deserialize)]
struct MetadataDoc {
    packages: Vec<MetadataPackage>,
    workspace_members: Vec<String>,
    /// The absolute path of the workspace root `cargo metadata`
    /// resolved.
    ///
    /// The directory whose `.cargo/config.toml` is the workspace's —
    /// the file the bridge carve-out reads, located from the metadata
    /// itself so the check's launch directory cannot redirect it.
    workspace_root: String,
}

#[derive(Debug, serde::Deserialize)]
struct MetadataPackage {
    name: String,
    /// The `cargo metadata` id of this package.
    ///
    /// The membership list the same document carries is expressed in
    /// this form, so deciding whether a package belongs to the
    /// workspace is a direct id comparison.
    id: String,
    /// Where the package is fetched from, when it is fetched at all.
    ///
    /// `cargo metadata` reports `null` for a crate read off the local
    /// filesystem, which is the fact that separates a workspace member
    /// and a foreign path dependency from a published crate.
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    dependencies: Vec<MetadataDep>,
}

#[derive(Debug, serde::Deserialize)]
struct MetadataDep {
    name: String,
    /// The dependency kind, `None` for a runtime dependency.
    ///
    /// `"dev"` and `"build"` mark edges that do not ship; they are
    /// filtered out while the [`Package`] view is built, so a dev-only
    /// coupling never reads as a boundary violation.
    #[serde(default)]
    kind: Option<String>,
}

/// Converts the parsed `cargo metadata` document into the [`Package`] view the
/// [`Checker`] consumes, dropping dev- and build-dependencies.
fn packages_from_metadata(doc: MetadataDoc) -> Vec<Package> {
    let MetadataDoc {
        packages,
        workspace_members,
        ..
    } = doc;
    packages
        .into_iter()
        .map(|p| Package {
            name: p.name,
            deps: p
                .dependencies
                .into_iter()
                .filter(|d| d.kind.is_none())
                .map(|d| d.name)
                .collect(),
            source: p.source,
            workspace_member: workspace_members.contains(&p.id),
        })
        .collect()
}

/// Runs the boundary check over this workspace and reports the verdict.
///
/// Asks `cargo metadata` for the graph, converts it into the [`Package`]
/// view the rules read, and prints every violation to stderr. Exits `0`
/// on a clean workspace, `1` when a rule is violated, and `3` when the
/// metadata could not be obtained — a broken toolchain should not look
/// like a broken manifest.
pub fn run_check() -> ExitCode {
    let output = Command::new("cargo")
        .arg("metadata")
        .arg("--format-version")
        .arg("1")
        .output();
    let output = match output {
        Ok(o) if o.status.success() => o.stdout,
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            eprintln!("xtask: `cargo metadata` failed:\n{stderr}");
            return ExitCode::from(3);
        }
        Err(e) => {
            eprintln!("xtask: failed to spawn `cargo metadata`: {e}");
            return ExitCode::from(3);
        }
    };

    let doc: MetadataDoc = match serde_json::from_slice(&output) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("xtask: failed to parse `cargo metadata` output: {e}");
            return ExitCode::from(3);
        }
    };

    // The bridge config lives at the workspace root; `cargo
    // metadata` names that root whatever directory the check was
    // launched from, so the read cannot land on a member's config.
    let config_path = std::path::Path::new(&doc.workspace_root).join(".cargo/config.toml");
    let pkgs = packages_from_metadata(doc);
    let bridge = local_bridge_declared(&config_path);
    let checker = Checker::new(&pkgs);
    let violations = if bridge {
        checker.check_with_local_bridge(true)
    } else {
        checker.check()
    };

    if bridge {
        for pkg in pkgs
            .iter()
            .filter(|p| p.source.is_none() && !p.workspace_member)
        {
            if is_bridge_family(&pkg.name) {
                println!(
                    "xtask: local bridge active — {} resolves by path via \
                     .cargo/config.toml (uncommitted; not a clean-graph pass)",
                    pkg.name
                );
            }
        }
    }
    if violations.is_empty() {
        println!("xtask: boundary check passed (5 rules, 0 violations)");
        ExitCode::SUCCESS
    } else {
        eprintln!(
            "xtask: boundary check FAILED ({} violation(s)):",
            violations.len()
        );
        for v in &violations {
            eprintln!("  - {}: {}", v.crate_name, v.detail);
        }
        ExitCode::FAILURE
    }
}

#[cfg(test)]
#[allow(
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::expect_used,
    clippy::unwrap_used
)]
mod tests {
    use super::*;

    /// The `source` string a published crate carries in metadata output.
    ///
    /// Used by [`pkg`] so the fixture's non-members read as fetched
    /// rather than as the foreign path dependencies rule 5 reports.
    const REGISTRY_SOURCE: &str = "registry+https://github.com/rust-lang/crates.io-index";

    fn pkg(name: &str, deps: &[&str]) -> Package {
        let member = is_dch_crate(name);
        Package {
            name: name.into(),
            deps: deps.iter().map(|s| (*s).to_string()).collect(),
            source: (!member).then(|| REGISTRY_SOURCE.to_string()),
            workspace_member: member,
        }
    }

    /// A dependency declared by path outside the workspace.
    ///
    /// The shape rule 5 exists to catch, and the one a sibling checkout
    /// invites: it resolves only on the machine holding that checkout.
    fn path_pkg(name: &str, deps: &[&str]) -> Package {
        Package {
            name: name.into(),
            deps: deps.iter().map(|s| (*s).to_string()).collect(),
            source: None,
            workspace_member: false,
        }
    }

    /// Returns a copy of `pkgs` with the package named `name` replaced by
    /// `replacement`. If `name` isn't found, returns the slice unchanged (the
    /// test would then fail its assertion, surfacing the typo).
    fn with_pkg(pkgs: &[Package], name: &str, replacement: &Package) -> Vec<Package> {
        pkgs.iter()
            .map(|p| {
                if p.name == name {
                    replacement.clone()
                } else {
                    p.clone()
                }
            })
            .collect()
    }

    /// A workspace that satisfies all five rules.
    fn good_workspace() -> Vec<Package> {
        vec![
            pkg(
                "dch",
                &["dch-config", "dch-loop", "dch-tui", "loopctl", "clap"],
            ),
            pkg("dch-config", &["loopctl", "serde"]),
            pkg("dch-loop", &["dch-config", "dch-tools", "loopctl", "tokio"]),
            pkg("dch-tools", &["loopctl", "serde_json", "syn"]),
            pkg("dch-tui", &["dch-config", "loopctl", "ratatui"]),
            pkg("loopctl", &["serde", "tokio"]),
        ]
    }

    fn has_violation(v: &[Violation], crate_name: &str, detail_substr: &str) -> bool {
        v.iter()
            .any(|x| x.crate_name == crate_name && x.detail.contains(detail_substr))
    }

    #[test]
    fn passes_on_clean_workspace() {
        let v = Checker::new(&good_workspace()).check();
        assert!(v.is_empty(), "expected no violations, got {v:?}");
    }

    #[test]
    fn bridge_declaration_reads_content_not_existence() {
        // The carve-out keys on the patch-table header, nothing else:
        // an aliases-only config — the file exists, the bridge does
        // not — must not relax the foreign-path rule, while the
        // section's exact spelling declares. A revert to
        // `.exists()`-style behavior cannot pass this.
        let bridge = "# local-only\n[patch.\"https://github.com/dch-labs/loopctl.git\"]\nloopctl = { path = \"../loopctl\" }\n";
        assert!(
            bridge_declared_in(bridge),
            "the patch section declares the bridge"
        );
        let aliases =
            "[alias]\nck = \"check\"\n[target.x86_64-unknown-linux-gnu]\nlinker = \"clang\"\n";
        assert!(
            !bridge_declared_in(aliases),
            "an aliases-only config declares nothing"
        );
        assert!(!bridge_declared_in(""), "an empty config declares nothing");
        assert!(
            !bridge_declared_in("[patch.\"https://github.com/other/dep.git\"]"),
            "a foreign patch section is not this bridge"
        );
        assert!(
            !bridge_declared_in("# [patch.\"https://github.com/dch-labs/loopctl.git\"]"),
            "a commented-out header is not a table Cargo honors"
        );
        assert!(
            !bridge_declared_in(
                "note = \"the header [patch.\"https://github.com/dch-labs/loopctl.git\"] is retired\""
            ),
            "the header's text inside a string value is not a declaration"
        );
        assert!(
            !bridge_declared_in("[patch."),
            "unparseable text declares nothing"
        );
    }

    #[test]
    fn the_bridge_read_targets_the_path_it_was_handed() {
        // The carve-out reads the path the metadata front-end derives
        // — the workspace root's config — never the process's current
        // directory: a config at a named path declares through that
        // path, and a directory without one declares nothing however
        // the CWD is set.
        let dir = tempfile::TempDir::new().expect("tempdir");
        let config_dir = dir.path().join(".cargo");
        std::fs::create_dir_all(&config_dir).expect("config dir");
        std::fs::write(
            config_dir.join("config.toml"),
            "[patch.\"https://github.com/dch-labs/loopctl.git\"]\nloopctl = { path = \"../loopctl\" }\n",
        )
        .expect("config written");
        assert!(
            local_bridge_declared(&config_dir.join("config.toml")),
            "the named path's config declares through itself"
        );
        let bare = tempfile::TempDir::new().expect("tempdir");
        assert!(
            !local_bridge_declared(&bare.path().join(".cargo/config.toml")),
            "a directory without a config declares nothing"
        );
    }

    #[test]
    fn an_external_path_dependency_violates_the_source_rule() {
        let sibling = path_pkg("loopctl", &["serde", "tokio"]);
        let ws = with_pkg(&good_workspace(), "loopctl", &sibling);
        let v = Checker::new(&ws).check();
        assert!(
            has_violation(&v, "loopctl", "outside the workspace"),
            "a sibling path dependency is reported: {v:?}"
        );
    }

    #[test]
    fn a_fetched_dependency_satisfies_the_source_rule() {
        let fetched = pkg("loopctl", &["serde", "tokio"]);
        assert!(
            fetched.source.is_some(),
            "the fixture models a crate resolved from a published source"
        );
        let ws = with_pkg(&good_workspace(), "loopctl", &fetched);
        let v = Checker::new(&ws).check();
        assert!(
            v.is_empty(),
            "the same dependency fetched passes all rules: {v:?}"
        );
    }

    #[test]
    fn the_local_bridge_sanctions_only_the_loopctl_family() {
        let mut ws = with_pkg(
            &good_workspace(),
            "loopctl",
            &path_pkg("loopctl", &["serde", "tokio"]),
        );
        ws.push(path_pkg("loopctl-derive", &["syn"]));
        let bridged = Checker::new(&ws).check_with_local_bridge(true);
        assert!(
            bridged.is_empty(),
            "the declared bridge sanctions the loopctl family resolving by \
             path: {bridged:?}"
        );
        let unbridged = Checker::new(&ws).check_with_local_bridge(false);
        assert!(
            has_violation(&unbridged, "loopctl", "outside the workspace")
                && has_violation(&unbridged, "loopctl-derive", "outside the workspace"),
            "without the declaration the same graph violates rule 5 for both: \
             {unbridged:?}"
        );
    }

    #[test]
    fn the_local_bridge_never_sanctions_other_path_deps() {
        let mut ws = with_pkg(
            &good_workspace(),
            "loopctl",
            &path_pkg("loopctl", &["serde", "tokio"]),
        );
        ws.push(path_pkg("some-other-sibling", &["serde"]));
        let v = Checker::new(&ws).check_with_local_bridge(true);
        assert!(
            has_violation(&v, "some-other-sibling", "outside the workspace"),
            "the bridge cannot launder a path dependency outside the loopctl \
             family: {v:?}"
        );
        assert!(
            !has_violation(&v, "loopctl", "outside the workspace"),
            "the family itself stays sanctioned beside the stranger: {v:?}"
        );
    }

    #[test]
    fn rule1_catches_dch_tools_gaining_dch_dep() {
        let bad = pkg("dch-tools", &["loopctl", "serde_json", "syn", "dch-config"]);
        let ws = with_pkg(&good_workspace(), "dch-tools", &bad);
        let v = Checker::new(&ws).check();
        assert!(has_violation(&v, "dch-tools", "dch-config"));
    }

    #[test]
    fn rule2_catches_dch_tui_gaining_dch_tools() {
        let bad = pkg(
            "dch-tui",
            &["dch-config", "dch-tools", "loopctl", "ratatui"],
        );
        let ws = with_pkg(&good_workspace(), "dch-tui", &bad);
        let v = Checker::new(&ws).check();
        assert!(has_violation(&v, "dch-tui", "dch-tools"));
    }

    #[test]
    fn rule2_catches_dch_tui_losing_dch_config() {
        let bad = pkg("dch-tui", &["loopctl", "ratatui"]);
        let ws = with_pkg(&good_workspace(), "dch-tui", &bad);
        let v = Checker::new(&ws).check();
        assert!(has_violation(&v, "dch-tui", "dch-config"));
    }

    #[test]
    fn rule3_catches_loopctl_gaining_dch_dep() {
        let bad = pkg("loopctl", &["serde", "tokio", "dch-tools"]);
        let ws = with_pkg(&good_workspace(), "loopctl", &bad);
        let v = Checker::new(&ws).check();
        assert!(has_violation(&v, "loopctl", "one-way"));
    }

    #[test]
    fn rule4_catches_dch_loop_gaining_extra_dep() {
        let bad = pkg(
            "dch-loop",
            &["dch-config", "dch-tools", "dch-tui", "loopctl", "tokio"],
        );
        let ws = with_pkg(&good_workspace(), "dch-loop", &bad);
        let v = Checker::new(&ws).check();
        assert!(has_violation(&v, "dch-loop", "dch-tui"));
    }

    #[test]
    fn rule4_catches_dch_binary_gaining_extra_dep() {
        let bad = pkg(
            "dch",
            &[
                "dch-config",
                "dch-loop",
                "dch-tui",
                "dch-tools",
                "loopctl",
                "clap",
            ],
        );
        let ws = with_pkg(&good_workspace(), "dch", &bad);
        let v = Checker::new(&ws).check();
        assert!(has_violation(&v, "dch", "dch-tools"));
    }

    #[test]
    fn dev_and_build_deps_are_not_runtime_edges() -> Result<(), serde_json::Error> {
        let json = r#"{
            "packages": [
                {
                    "name": "fake",
                    "id": "path+file:///tmp/fake#0.1.0",
                    "dependencies": [
                        {"name": "dch-config"},
                        {"name": "dch-tools", "kind": "dev"},
                        {"name": "dch-tui",   "kind": "build"}
                    ]
                }
            ],
            "workspace_members": [],
            "workspace_root": "/tmp/ws"
        }"#;
        let doc: MetadataDoc = serde_json::from_str(json)?;
        let pkgs = packages_from_metadata(doc);
        // Exactly one package in the fixture; its only normal dep is dch-config.
        // The dev-dep on dch-tools and the build-dep on dch-tui must be filtered.
        assert_eq!(
            pkgs.as_slice(),
            [Package {
                name: "fake".to_string(),
                deps: vec!["dch-config".to_string()],
                source: None,
                workspace_member: false,
            }]
        );
        Ok(())
    }
}
