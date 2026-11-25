//! Persisting the config document: the atomic, owner-only
//! write-through.
//!
//! [`DchConfig::save_to_dir`] validates the document, stages it beside
//! its target, and renames it into place; [`DchConfig::save`] points
//! that at the user's config directory.

use crate::DchConfig;
use crate::DchConfigError;
use crate::config_dir;

use std::path::Path;

impl DchConfig {
    /// Persist the configuration to `dir`, atomically.
    ///
    /// Write-through for every value a live session changed: the
    /// document is the full configuration via
    /// [`to_toml_string`](Self::to_toml_string), and the target follows
    /// the load precedence — `config.local.toml` when one stands, else
    /// `config.toml` — so what the next launch loads is what this
    /// session left. The local-file preference is sampled at save
    /// time, and the check carries a small race by design: a local
    /// file created in the window between the check and the write
    /// lands shadows this session's write in `config.toml` until the
    /// next save finds the local file and targets it — the accepted
    /// cost of never creating a local file on a session's behalf. The
    /// write is atomic: the document lands in a
    /// sibling temporary file first, its bytes are pushed to the disk,
    /// and the file renames into place — so a crashed process leaves
    /// the previous file whole rather than a torn document, and a
    /// power loss in the window cannot leave a zero-length live file
    /// where the staged bytes never settled. The staged file is
    /// *created* with the owner-only mode it must keep — the document
    /// may carry a credential, and a rename replaces the target's
    /// mode, so the staging step is where a live change would
    /// otherwise loosen a chmod-600 config to world-readable; creating
    /// it owner-only from the first byte leaves no window where the
    /// credential is wider, and no crash path that strands it wider.
    /// The document is validated before anything is staged: a save may
    /// never land a configuration the loader itself would refuse,
    /// because the next start would then die on this file until a hand
    /// edit fixed it. A symlinked target (stow, a dotfiles repo) is
    /// resolved before anything is staged: the rename replaces the
    /// file the link names, never the link itself, so the managed copy
    /// the user keeps is the one that changes. A staged write that
    /// fails removes itself — a stray can carry a credential, and its
    /// pid-and-sequence name could collide with a pid-reusing
    /// successor (a container's dch is often pid 1 against a
    /// persistent home) — and the name carries a time nonce so even a
    /// stray left by a crash cannot wedge a later save.
    ///
    /// # Errors
    ///
    /// Returns an error when validation refuses the document, when
    /// serialization fails, when the directory cannot be created, or
    /// when the staged write or rename does — the target file itself is
    /// never touched until the rename succeeds.
    pub fn save_to_dir(&self, dir: &Path) -> Result<(), DchConfigError> {
        static SAVE_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        self.validate()?;
        let document = self.to_toml_string()?;
        let chosen = if dir.join("config.local.toml").exists() {
            dir.join("config.local.toml")
        } else {
            dir.join("config.toml")
        };
        std::fs::create_dir_all(dir)?;
        let target = std::fs::canonicalize(&chosen).unwrap_or(chosen);
        let staging = target.with_file_name(format!(
            ".{}.{}.{}.{}.tmp",
            target
                .file_name()
                .and_then(std::ffi::OsStr::to_str)
                .unwrap_or("config.toml"),
            std::process::id(),
            SAVE_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            Self::staging_nonce(),
        ));
        Self::write_staging_file(&staging, &document)?;
        if let Err(err) = std::fs::rename(&staging, &target) {
            drop(std::fs::remove_file(&staging));
            return Err(err.into());
        }
        Ok(())
    }

    /// A time-based nonce for the staging name.
    ///
    /// The pid and the per-process sequence alone collide with a stray
    /// left by an earlier process whose pid a container reuses — every
    /// fresh container is pid 1 against the same persistent home. The
    /// nonce makes the name unique across process generations, so a
    /// stale `.tmp` can never refuse a successor's first save.
    fn staging_nonce() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| {
                u64::try_from(since.as_nanos()).unwrap_or(u64::MAX)
            })
    }

    /// Persist the configuration to the user's [config dir](config_dir).
    ///
    /// The production half of write-through: hosts call this after a
    /// live change so the next launch opens where this session left
    /// off. See [`save_to_dir`](Self::save_to_dir) for the file choice
    /// and the atomicity contract.
    ///
    /// # Errors
    ///
    /// See [`save_to_dir`](Self::save_to_dir).
    pub fn save(&self) -> Result<(), DchConfigError> {
        self.save_to_dir(&config_dir())
    }

    /// Create the staging file with the mode it must keep, write the
    /// document through it, and flush the bytes to the disk before the
    /// caller's rename.
    ///
    /// The staging half of [`save_to_dir`](Self::save_to_dir)'s
    /// atomicity: on unix the file is created `0600` — owner-only from
    /// its first byte, so the credential the document may carry is
    /// never readable by another local user, not in the write window a
    /// chmod-after-create would leave, not in the stray a crash in that
    /// window would strand. `create_new` refuses to touch anything
    /// already standing at the path, which the caller's unique staging
    /// name makes a should-never; refusing is the honest answer if it
    /// ever happens. The `sync_all` before the return is the
    /// power-loss half: a rename replaces the target by directory
    /// entry, but on several common filesystems the entry can settle
    /// before the staged bytes do, leaving a zero-length live file
    /// after a crash — pushing the data out first closes that window,
    /// so the atomicity the caller claims holds against the wall, not
    /// only the process. Platforms without file modes write plainly —
    /// the mode contract is unix's to keep.
    ///
    /// # Errors
    ///
    /// Returns the filesystem error the create, the write, or the
    /// sync raises; a failed write or sync removes the staging file it
    /// created, so nothing partial and nothing secret is stranded — an
    /// open that itself fails owns no file and removes nothing.
    fn write_staging_file(staging: &Path, document: &str) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::io::Write as _;
            use std::os::unix::fs::OpenOptionsExt as _;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(staging)?;
            if let Err(err) = file
                .write_all(document.as_bytes())
                .and_then(|()| file.sync_all())
            {
                drop(std::fs::remove_file(staging));
                return Err(err);
            }
            Ok(())
        }
        #[cfg(not(unix))]
        {
            std::fs::write(staging, document)
        }
    }
}

#[cfg(test)]
mod save_roundtrip {
    use super::*;
    use crate::Verbosity;

    #[test]
    fn a_saved_configuration_loads_back_identical() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = DchConfig::default();
        config.api.model = "switched-model".to_string();
        config.display.theme = "nord".to_string();
        config.display.verbosity = Verbosity::Verbose;
        config.save_to_dir(dir.path()).expect("the save lands");
        let back = DchConfig::load_from_dir(dir.path()).expect("the save loads back");
        assert_eq!(
            back.to_toml_string().expect("loads serialize"),
            config.to_toml_string().expect("original serializes"),
            "save then load is a lossless round trip"
        );
        assert_eq!(
            back.api.model, "switched-model",
            "the changed field arrives"
        );
        assert_eq!(back.display.theme, "nord", "the theme arrives");
    }

    #[cfg(unix)]
    #[test]
    fn a_save_through_a_symlink_replaces_the_managed_file_not_the_link() {
        // A stow- or dotfiles-managed config is a symlink; the rename
        // must replace the file the link names, never the link itself —
        // detaching it would leave the managed copy stale while the
        // next start loads a different file than the user manages.
        let managed = tempfile::tempdir().expect("managed tempdir");
        let real = managed.path().join("real-config.toml");
        std::fs::write(&real, "[api]\nmodel = \"old\"\n").expect("managed file");
        let dir = tempfile::tempdir().expect("config tempdir");
        std::os::unix::fs::symlink(&real, dir.path().join("config.toml")).expect("symlink");

        let mut config = DchConfig::default();
        config.api.model = "new".to_string();
        config.save_to_dir(dir.path()).expect("the save lands");

        let link =
            std::fs::symlink_metadata(dir.path().join("config.toml")).expect("the link stands");
        assert!(
            link.file_type().is_symlink(),
            "the save replaced the managed file, not the link"
        );
        let saved = std::fs::read_to_string(&real).expect("the managed file");
        assert!(
            saved.contains("\"new\""),
            "the managed copy changed: {saved}"
        );
    }

    #[test]
    fn a_stray_staging_file_cannot_wedge_a_later_save() {
        // The pid-and-sequence name alone collides with a stray left by
        // a crashed process whose pid a container reuses: the successor
        // creates `create_new` and fails AlreadyExists forever. The
        // nonce makes the fresh save's staging name its own, so the
        // stray sits untouched beside a save that lands anyway.
        let dir = tempfile::tempdir().expect("tempdir");
        let stray = dir
            .path()
            .join(format!(".config.toml.{}.0.tmp", std::process::id()));
        std::fs::write(&stray, "half a document from a crashed process").expect("stray");

        let mut config = DchConfig::default();
        config.api.model = "fresh".to_string();
        config
            .save_to_dir(dir.path())
            .expect("the save lands beside the stray");

        let landed = std::fs::read_to_string(dir.path().join("config.toml")).expect("the target");
        assert!(
            landed.contains("\"fresh\""),
            "the new document arrived: {landed}"
        );
        assert_eq!(
            std::fs::read_to_string(&stray).expect("the stray stands"),
            "half a document from a crashed process",
            "the stray is untouched — the fresh save never tried its name"
        );
    }

    #[test]
    fn a_rename_that_cannot_land_cleans_up_its_staging_file() {
        // The staged file is this save's own: a rename that fails (here
        // the target name is taken by a directory) must remove it, or
        // the directory keeps a stray that may carry a credential.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("config.toml")).expect("a directory target");

        let mut config = DchConfig::default();
        config.api.model = "wedged".to_string();
        assert!(
            config.save_to_dir(dir.path()).is_err(),
            "a rename onto a directory fails"
        );
        let strays: Vec<_> = std::fs::read_dir(dir.path())
            .expect("the dir reads")
            .filter_map(Result::ok)
            .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "tmp"))
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            strays.is_empty(),
            "the failed save strands no staging file: {strays:?}"
        );
    }

    #[test]
    fn concurrent_savers_never_land_a_torn_document() {
        // Two savers racing through one directory must degrade to
        // last-writer-wins, never to a hybrid: each save stages under
        // its own name, so no interleaving can leave half of one
        // document inside another's rename.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut handles = Vec::new();
        for which in 0..2_u64 {
            let root = dir.path().to_path_buf();
            handles.push(std::thread::spawn(move || {
                for round in 0..40_u64 {
                    let mut config = DchConfig::default();
                    config.api.model = format!("model-{which}-{round}");
                    config
                        .save_to_dir(&root)
                        .unwrap_or_else(|err| panic!("save {which}/{round} lands: {err}"));
                }
            }));
        }
        for handle in handles {
            handle.join().expect("both savers finish");
        }
        let landed = std::fs::read_to_string(dir.path().join("config.toml"))
            .expect("a whole document landed");
        let parsed = toml::from_str::<DchConfig>(&landed)
            .expect("the winner is one writer's whole document, never a splice");
        assert!(
            parsed.api.model.starts_with("model-"),
            "the landed model is one of the savers' own: {}",
            parsed.api.model
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_save_lands_the_config_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        // A pre-existing world-readable target: the staged replacement
        // must tighten it, not carry the loose mode into place.
        std::fs::write(dir.path().join("config.toml"), "[api]\n").expect("the target stands");
        std::fs::set_permissions(
            dir.path().join("config.toml"),
            std::fs::Permissions::from_mode(0o644),
        )
        .expect("the loose mode stands");
        DchConfig::default()
            .save_to_dir(dir.path())
            .expect("the save lands");
        let mode = std::fs::metadata(dir.path().join("config.toml"))
            .expect("the file stands")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "the saved config is owner-only — a live change never loosens it"
        );
    }

    #[test]
    fn a_save_targets_the_local_file_when_one_stands() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("config.local.toml"),
            "[api]\nmodel = \"local\"\n",
        )
        .expect("the local file stands");
        let mut config = DchConfig::default();
        config.api.model = "written-through".to_string();
        config.save_to_dir(dir.path()).expect("the save lands");
        let contents =
            std::fs::read_to_string(dir.path().join("config.local.toml")).expect("local exists");
        assert!(
            contents.contains("written-through"),
            "the local file — the one a load prefers — carries the change: {contents}"
        );
        assert!(
            !dir.path().join("config.toml").exists(),
            "no config.toml is created beside a standing local file"
        );
    }

    #[test]
    fn a_first_save_creates_the_canonical_file_and_leaves_no_temp() {
        let dir = tempfile::tempdir().expect("tempdir");
        DchConfig::default()
            .save_to_dir(dir.path())
            .expect("the save lands");
        assert!(
            dir.path().join("config.toml").exists(),
            "the canonical file appears"
        );
        let strays: Vec<_> = std::fs::read_dir(dir.path())
            .expect("the dir reads")
            .filter_map(std::result::Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.rsplit('.').next() == Some("tmp"))
            })
            .collect();
        assert!(
            strays.is_empty(),
            "the staging file renamed away, none left behind"
        );
    }

    #[test]
    fn a_save_refuses_a_document_the_loader_would_refuse() {
        // The write-through's own guard: a config mutated past the
        // loader's bounds stops at the save, never at the next start —
        // a save that landed it would brick the launch until a hand
        // edit fixed the file. Nothing is staged and the target keeps
        // whatever it carried.
        let dir = tempfile::tempdir().expect("tempdir");
        let mut config = DchConfig::default();
        config.runner.turn_timeout_secs = 60;
        let err = config
            .save_to_dir(dir.path())
            .expect_err("the loader refuses this document; the save must refuse it first");
        assert!(
            err.to_string().contains("turn_timeout_secs"),
            "the refusal names the field: {err}"
        );
        assert!(
            !dir.path().join("config.toml").exists(),
            "a refused save touches no target"
        );
        let entries = std::fs::read_dir(dir.path())
            .expect("the dir reads")
            .filter_map(std::result::Result::ok)
            .count();
        assert_eq!(entries, 0, "a refused save stages nothing");
    }

    #[test]
    #[cfg(unix)]
    fn a_staging_file_is_created_owner_only() {
        // The document may carry a credential, so the staging file is
        // created with the mode it must keep rather than created loose
        // and tightened after: under the usual umask a plain create
        // would stand at 0644 for the whole write window, and a crash
        // there would strand it that way for good.
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let staging = dir.path().join(".config.toml.stage");
        DchConfig::write_staging_file(&staging, "[api]\nmodel = \"m\"\n")
            .expect("the staging write lands");
        let mode = std::fs::metadata(&staging)
            .expect("the staging file stands")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "owner-only from the first byte, whatever the umask leaves open"
        );
    }
}
