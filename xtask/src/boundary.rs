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

/// One package as seen in `cargo metadata` output — the only fields we need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    /// Crate name (e.g. `"dch-tools"`, `"loopctl"`).
    pub name: String,
    /// Declared dependency names (workspace and external alike).
    pub deps: Vec<String>,
}

/// A single boundary-rule violation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    /// The crate whose dependency list violates a rule.
    pub crate_name: String,
    /// Human-readable description of the violation.
    pub detail: String,
}

/// Returns true iff `dep` names a dch workspace crate (`dch`, `dch-config`, …).
fn is_dch_crate(dep: &str) -> bool {
    dep == "dch" || dep.starts_with("dch-")
}

/// Selects only the dch-* dependencies of `pkg`.
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

/// Looks up a package by name in the slice.
fn find<'a>(pkgs: &'a [Package], name: &str) -> Option<&'a Package> {
    pkgs.iter().find(|p| p.name == name)
}

/// Pure boundary checker. Owns no I/O — testable with synthetic packages.
pub struct Checker<'a> {
    pkgs: &'a [Package],
}

impl<'a> Checker<'a> {
    /// Construct a checker over a slice of packages.
    pub fn new(pkgs: &'a [Package]) -> Self {
        Self { pkgs }
    }

    /// Run all four rules, collecting every violation. Empty Vec = pass.
    pub fn check(&self) -> Vec<Violation> {
        let mut out = Vec::new();
        out.extend(self.rule_dch_tools_loopctl_only());
        out.extend(self.rule_dch_tui_dch_config_only());
        out.extend(self.rule_loopctl_no_dch());
        out.extend(self.rule_exact_dch_dep_sets());
        out
    }

    /// Rule 1: `dch-tools` depends only on `loopctl` (no `dch-*` crate).
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

    /// Rule 2: `dch-tui`'s dch-* deps == `{dch-config}` exactly.
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

    /// Rule 3: `loopctl` depends on no `dch-*` crate (one-way boundary, P1).
    fn rule_loopctl_no_dch(&self) -> Vec<Violation> {
        let Some(pkg) = find(self.pkgs, "loopctl") else {
            // loopctl is a path dep; with full metadata it must appear. If it
            // doesn't, that's a separate problem worth surfacing.
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

    /// Rule 4: exact dch-* dep-sets for `dch-loop` and the `dch` binary.
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
}

/// Shape of the subset of `cargo metadata` JSON we deserialize.
#[derive(Debug, serde::Deserialize)]
struct MetadataDoc {
    packages: Vec<MetadataPackage>,
}

#[derive(Debug, serde::Deserialize)]
struct MetadataPackage {
    name: String,
    #[serde(default)]
    dependencies: Vec<MetadataDep>,
}

#[derive(Debug, serde::Deserialize)]
struct MetadataDep {
    name: String,
}

/// Runs `cargo metadata`, parses it, and prints violations. Exit 0 on pass.
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

    let pkgs: Vec<Package> = doc
        .packages
        .into_iter()
        .map(|p| Package {
            name: p.name,
            deps: p.dependencies.into_iter().map(|d| d.name).collect(),
        })
        .collect();

    let violations = Checker::new(&pkgs).check();
    if violations.is_empty() {
        println!("xtask: boundary check passed (4 rules, 0 violations)");
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
#[allow(clippy::missing_panics_doc)]
mod tests {
    //! Each rule gets a passing case and a violating case, proving the gate
    //! actually bites.

    use super::*;

    fn pkg(name: &str, deps: &[&str]) -> Package {
        Package {
            name: name.into(),
            deps: deps.iter().map(|s| (*s).to_string()).collect(),
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

    /// A workspace that satisfies all four rules.
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
    fn rule1_catches_dch_tools_gaining_dch_dep() {
        let bad = pkg("dch-tools", &["loopctl", "serde_json", "syn", "dch-config"]);
        let ws = with_pkg(&good_workspace(), "dch-tools", &bad);
        let v = Checker::new(&ws).check();
        assert!(has_violation(&v, "dch-tools", "dch-config"));
    }

    // Rule 2: dch-tui's dch-* deps must be exactly {dch-config}.

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
}
