//! A shared, reloadable view of the config the server was started with.
//!
//! `dbs serve` used to load `dbs.toml` once and hand every handler the
//! same frozen `Arc<Config>`. The web UI also *edits* that file (add,
//! remove, enable/disable a source), so after any edit the server kept
//! serving its startup copy: a newly added source didn't show up, and
//! "Back up now" had nothing to run, until the process restarted.
//!
//! [`ConfigHandle`] keeps the same cheap-snapshot model — each request
//! takes one `Arc<Config>` and uses it throughout, so a request never
//! sees a half-applied change — but lets the editing handlers swap in a
//! freshly loaded copy with [`ConfigHandle::reload`].

use std::sync::{Arc, RwLock};

use dbs_core::{load_config, Config, DbsError};

/// Cloneable handle to the current config snapshot. Clones share state.
#[derive(Clone)]
pub(crate) struct ConfigHandle {
    current: Arc<RwLock<Arc<Config>>>,
}

impl ConfigHandle {
    /// Wraps the config loaded at startup.
    pub(crate) fn new(config: Config) -> Self {
        Self {
            current: Arc::new(RwLock::new(Arc::new(config))),
        }
    }

    /// The current snapshot. Cheap: clones an `Arc`, not the config.
    pub(crate) fn current(&self) -> Arc<Config> {
        // A poisoned lock only means a writer panicked mid-swap; the
        // stored `Arc` is still a complete config, so keep serving it.
        let guard = self.current.read().unwrap_or_else(|e| e.into_inner());
        Arc::clone(&guard)
    }

    /// Re-reads the config file this snapshot was loaded from and makes
    /// the result current. A config built in memory (no `source_path`,
    /// as in tests) has nothing to re-read, so it stays as it is.
    ///
    /// On a load error the previous snapshot is kept and the error is
    /// returned, so a bad hand-edit can't take the running server down.
    pub(crate) fn reload(&self) -> Result<(), DbsError> {
        let Some(path) = self.current().source_path.clone() else {
            return Ok(());
        };
        let fresh = Arc::new(load_config(&path)?);
        let mut guard = self.current.write().unwrap_or_else(|e| e.into_inner());
        *guard = fresh;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_config_file(label: &str, body: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dbs-web-config-handle-{label}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dbs.toml");
        std::fs::write(&path, body).unwrap();
        path
    }

    const BASE: &str = "[dbs]\ndatabase = \"dbs.sqlite3\"\n";

    #[test]
    fn reload_picks_up_a_source_added_to_the_file() {
        let path = temp_config_file("adds", BASE);
        let handle = ConfigHandle::new(load_config(&path).unwrap());
        assert!(handle.current().sources.is_empty());

        std::fs::write(
            &path,
            format!("{BASE}[sources.raindrop]\ntype = \"raindrop\"\n"),
        )
        .unwrap();
        handle.reload().unwrap();

        assert!(handle.current().sources.contains_key("raindrop"));
    }

    #[test]
    fn a_snapshot_taken_before_reload_is_unchanged() {
        let path = temp_config_file("snapshot", BASE);
        let handle = ConfigHandle::new(load_config(&path).unwrap());
        let before = handle.current();

        std::fs::write(&path, format!("{BASE}[sources.a]\ntype = \"raindrop\"\n")).unwrap();
        handle.reload().unwrap();

        assert!(
            before.sources.is_empty(),
            "in-flight requests keep their snapshot"
        );
        assert_eq!(handle.current().sources.len(), 1);
    }

    #[test]
    fn a_broken_file_keeps_the_previous_config_and_reports_the_error() {
        let path = temp_config_file(
            "broken",
            &format!("{BASE}[sources.a]\ntype = \"raindrop\"\n"),
        );
        let handle = ConfigHandle::new(load_config(&path).unwrap());

        std::fs::write(&path, "this is = = not toml").unwrap();
        assert!(handle.reload().is_err());

        assert!(handle.current().sources.contains_key("a"));
    }

    #[test]
    fn clones_share_the_reloaded_config() {
        let path = temp_config_file("clones", BASE);
        let handle = ConfigHandle::new(load_config(&path).unwrap());
        let other = handle.clone();

        std::fs::write(&path, format!("{BASE}[sources.a]\ntype = \"raindrop\"\n")).unwrap();
        handle.reload().unwrap();

        assert!(other.current().sources.contains_key("a"));
    }

    #[test]
    fn reload_without_a_source_path_is_a_no_op() {
        let mut config = load_config(&temp_config_file("in-memory", BASE)).unwrap();
        config.source_path = None;
        let handle = ConfigHandle::new(config);
        assert!(handle.reload().is_ok());
        assert!(handle.current().sources.is_empty());
    }
}
