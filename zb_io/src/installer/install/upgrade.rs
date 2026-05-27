use std::sync::Arc;

use zb_core::Error;

use super::{Installer, acquire_install_lock};
use crate::progress::ProgressCallback;

impl Installer {
    /// Upgrade an installed package to its latest version.
    ///
    /// Flow: hold the install lock for the whole operation, snapshot the
    /// currently-installed version, plan the new version, then **uninstall
    /// the old version before installing the new one**. The uninstall-first
    /// order matters: a fresh install would otherwise hit `LinkConflict` on
    /// the old version's symlinks, and would leave the old cellar directory
    /// behind on disk (the leak this method exists to fix).
    ///
    /// If the planner finds the package is already on the latest version
    /// (`plan.items` is empty), this is a no-op that returns `Ok(())`.
    pub async fn upgrade(
        &mut self,
        name: &str,
        build_from_source: bool,
        link: bool,
        progress: Option<Arc<ProgressCallback>>,
    ) -> Result<(), Error> {
        // One lock for the entire flow — uninstall + install must not race
        // with other zb processes touching the same package.
        let _lock = acquire_install_lock(&self.locks_dir)?;

        let old = self.db.get_installed(name).ok_or(Error::NotInstalled {
            name: name.to_string(),
        })?;

        let plan = self
            .plan_with_options(&[name.to_string()], build_from_source)
            .await?;

        // Empty plan = already on the latest version. No-op, keep installation.
        if plan.items.is_empty() {
            return Ok(());
        }

        // Clear old version first: unlinks symlinks, removes the keg directory,
        // drops `installed_kegs` / `keg_files` rows, decrements `store_refs`.
        // After this the new install path is unobstructed.
        self.uninstall_by_version(name, &old.version)?;

        // We already hold the lock, so call the no-lock variant.
        self.execute_inner(plan, link, progress).await?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::cellar::Cellar;
    use crate::installer::install::test_support::*;
    use crate::network::api::ApiClient;
    use crate::storage::blob::BlobCache;
    use crate::storage::db::Database;
    use crate::storage::store::Store;
    use crate::{Installer, Linker};

    fn formula_json(mock_uri: &str, name: &str, version: &str, tag: &str, sha: &str) -> String {
        format!(
            r#"{{
                "name": "{name}",
                "versions": {{ "stable": "{version}" }},
                "dependencies": [],
                "bottle": {{
                    "stable": {{
                        "files": {{
                            "{tag}": {{
                                "url": "{mock_uri}/bottles/{name}-{version}.{tag}.bottle.tar.gz",
                                "sha256": "{sha}"
                            }}
                        }}
                    }}
                }}
            }}"#
        )
    }

    fn make_installer(
        root: &std::path::Path,
        prefix: &std::path::Path,
        mock_uri: &str,
    ) -> Installer {
        fs::create_dir_all(root.join("db")).unwrap();
        let api_client = ApiClient::with_base_url(format!("{mock_uri}/formula")).unwrap();
        let blob_cache = BlobCache::new(&root.join("cache")).unwrap();
        let store = Store::new(root).unwrap();
        let cellar = Cellar::new(root).unwrap();
        let linker = Linker::new(prefix).unwrap();
        let db = Database::open(&root.join("db/zb.sqlite3")).unwrap();
        Installer::new(
            api_client,
            blob_cache,
            store,
            cellar,
            linker,
            db,
            prefix.to_path_buf(),
            root.join("locks"),
        )
    }

    #[tokio::test]
    async fn upgrade_replaces_old_version_and_cleans_up() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let tag = get_test_bottle_tag();

        let bottle = create_bottle_tarball("testpkg");
        let sha = sha256_hex(&bottle);

        Mock::given(method("GET"))
            .and(path("/formula/testpkg.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(formula_json(
                &mock_server.uri(),
                "testpkg",
                "1.0.0",
                tag,
                &sha,
            )))
            .up_to_n_times(1)
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/bottles/testpkg-1.0.0.{tag}.bottle.tar.gz")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bottle.clone()))
            .up_to_n_times(1)
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/formula/testpkg.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(formula_json(
                &mock_server.uri(),
                "testpkg",
                "2.0.0",
                tag,
                &sha,
            )))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/bottles/testpkg-2.0.0.{tag}.bottle.tar.gz")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bottle))
            .mount(&mock_server)
            .await;

        let root = tmp.path().join("zerobrew");
        let prefix = tmp.path().join("homebrew");
        let mut installer = make_installer(&root, &prefix, &mock_server.uri());

        installer
            .install(&["testpkg".to_string()], true)
            .await
            .unwrap();
        assert!(root.join("cellar/testpkg/1.0.0").exists());
        assert!(prefix.join("bin/testpkg").exists());

        installer
            .upgrade("testpkg", false, true, None)
            .await
            .unwrap();

        assert!(root.join("cellar/testpkg/2.0.0").exists());
        assert!(
            !root.join("cellar/testpkg/1.0.0").exists(),
            "old cellar dir must be removed"
        );

        let bin_link = prefix.join("bin/testpkg");
        assert!(bin_link.exists(), "new version must be linked");
        let target = fs::read_link(&bin_link).unwrap();
        let target_str = target.to_string_lossy();
        assert!(
            target_str.contains("2.0.0"),
            "symlink must point at 2.0.0, got {target_str}"
        );
        assert!(
            !target_str.contains("1.0.0"),
            "symlink must not point at 1.0.0"
        );

        let installed = installer.get_installed("testpkg").unwrap();
        assert_eq!(installed.version, "2.0.0");
    }

    #[tokio::test]
    async fn upgrade_with_no_link_does_not_create_symlinks() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let tag = get_test_bottle_tag();

        let bottle = create_bottle_tarball("nolinkpkg");
        let sha = sha256_hex(&bottle);

        Mock::given(method("GET"))
            .and(path("/formula/nolinkpkg.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(formula_json(
                &mock_server.uri(),
                "nolinkpkg",
                "1.0.0",
                tag,
                &sha,
            )))
            .up_to_n_times(1)
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/bottles/nolinkpkg-1.0.0.{tag}.bottle.tar.gz"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bottle.clone()))
            .up_to_n_times(1)
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/formula/nolinkpkg.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(formula_json(
                &mock_server.uri(),
                "nolinkpkg",
                "2.0.0",
                tag,
                &sha,
            )))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/bottles/nolinkpkg-2.0.0.{tag}.bottle.tar.gz"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bottle))
            .mount(&mock_server)
            .await;

        let root = tmp.path().join("zerobrew");
        let prefix = tmp.path().join("homebrew");
        let mut installer = make_installer(&root, &prefix, &mock_server.uri());

        installer
            .install(&["nolinkpkg".to_string()], true)
            .await
            .unwrap();
        assert!(prefix.join("bin/nolinkpkg").exists());

        installer
            .upgrade("nolinkpkg", false, false, None)
            .await
            .unwrap();

        assert!(root.join("cellar/nolinkpkg/2.0.0").exists());
        assert!(!root.join("cellar/nolinkpkg/1.0.0").exists());
        assert!(
            !prefix.join("bin/nolinkpkg").exists(),
            "no symlinks expected when link=false"
        );
    }

    #[tokio::test]
    async fn upgrade_no_op_when_already_latest() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let tag = get_test_bottle_tag();

        let bottle = create_bottle_tarball("steadypkg");
        let sha = sha256_hex(&bottle);

        Mock::given(method("GET"))
            .and(path("/formula/steadypkg.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(formula_json(
                &mock_server.uri(),
                "steadypkg",
                "1.0.0",
                tag,
                &sha,
            )))
            .mount(&mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!(
                "/bottles/steadypkg-1.0.0.{tag}.bottle.tar.gz"
            )))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(bottle))
            .up_to_n_times(1)
            .mount(&mock_server)
            .await;

        let root = tmp.path().join("zerobrew");
        let prefix = tmp.path().join("homebrew");
        let mut installer = make_installer(&root, &prefix, &mock_server.uri());

        installer
            .install(&["steadypkg".to_string()], true)
            .await
            .unwrap();

        installer
            .upgrade("steadypkg", false, true, None)
            .await
            .unwrap();

        assert!(root.join("cellar/steadypkg/1.0.0").exists());
        assert!(prefix.join("bin/steadypkg").exists());
        assert_eq!(
            installer.get_installed("steadypkg").unwrap().version,
            "1.0.0"
        );
    }

    #[tokio::test]
    async fn upgrade_errors_when_not_installed() {
        let mock_server = MockServer::start().await;
        let tmp = TempDir::new().unwrap();
        let root = tmp.path().join("zerobrew");
        let prefix = tmp.path().join("homebrew");
        let mut installer = make_installer(&root, &prefix, &mock_server.uri());

        let err = installer
            .upgrade("nonexistent", false, true, None)
            .await
            .unwrap_err();
        assert!(matches!(err, zb_core::Error::NotInstalled { .. }));
    }
}
