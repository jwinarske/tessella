//! Where the offline store goes, and the checks that make putting it there safe.
//!
//! The environment cases run in one test rather than several, because `set_var` is process-wide
//! and two tests changing it in parallel would read each other's values. That is also why the
//! library never reads it on its own behalf.

use std::path::Path;

use tessella_storage::store_path::{
    STORE_DIR_VAR, STORE_FILE_NAME, StorePathError, from_env, prepare,
};

/// The environment is consulted in the documented order, and only when asked.
///
/// `TESSELLA_STORE_DIR` wins outright; otherwise `XDG_DATA_HOME/tessella`; otherwise
/// `$HOME/.local/share/tessella`. **Data, not cache** — a cache directory is one the system may
/// empty at will, and a downloaded region is bandwidth somebody paid for.
#[test]
fn the_environment_is_read_in_order() {
    // SAFETY-adjacent: this is the only test that touches these variables, and it restores them.
    let saved: Vec<(&str, Option<std::ffi::OsString>)> = [STORE_DIR_VAR, "XDG_DATA_HOME", "HOME"]
        .iter()
        .map(|key| (*key, std::env::var_os(key)))
        .collect();

    let set = |key: &str, value: Option<&str>| unsafe {
        match value {
            Some(value) => std::env::set_var(key, value),
            None => std::env::remove_var(key),
        }
    };

    // The explicit variable wins over both.
    set(STORE_DIR_VAR, Some("/srv/maps"));
    set("XDG_DATA_HOME", Some("/home/u/.data"));
    set("HOME", Some("/home/u"));
    assert_eq!(from_env().expect("resolves"), Path::new("/srv/maps"));

    // Then XDG_DATA_HOME, with the directory name appended.
    set(STORE_DIR_VAR, None);
    assert_eq!(
        from_env().expect("resolves"),
        Path::new("/home/u/.data/tessella")
    );

    // Then HOME, at the XDG default location — `.local/share`, not `.cache`.
    set("XDG_DATA_HOME", None);
    assert_eq!(
        from_env().expect("resolves"),
        Path::new("/home/u/.local/share/tessella")
    );

    // An empty value is not a value: an exported-but-blank variable is the usual way a startup
    // script sets nothing, and treating it as a path puts the store at the filesystem root.
    set(STORE_DIR_VAR, Some(""));
    assert_eq!(
        from_env().expect("resolves"),
        Path::new("/home/u/.local/share/tessella"),
        "a blank variable was taken as a path"
    );

    // A relative path is refused rather than resolved against the working directory, which would
    // silently mean a different store per directory the process was started from.
    set(STORE_DIR_VAR, Some("maps"));
    assert!(matches!(from_env(), Err(StorePathError::Relative(_))));

    // And with nothing set at all there is nowhere to put it.
    set(STORE_DIR_VAR, None);
    set("HOME", None);
    assert!(matches!(from_env(), Err(StorePathError::Unlocatable)));

    for (key, value) in saved {
        set(key, value.as_deref().and_then(|value| value.to_str()));
    }
}

/// A prepared directory exists, is owner-only, and names the store file inside it.
#[test]
fn preparing_creates_a_private_directory() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let store = dir.path().join("nested").join("tessella");

    let file = prepare(&store).expect("prepares");
    assert_eq!(file, store.join(STORE_FILE_NAME));
    assert!(store.is_dir());
    assert!(!file.exists(), "the file is the cache's to create");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(&store)
            .expect("stats")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700, "{mode:o}");
    }
}

/// Preparing an existing directory is not an error, and does not change it.
#[test]
fn preparing_an_existing_directory_is_idempotent() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let store = dir.path().join("tessella");
    let first = prepare(&store).expect("prepares");
    let second = prepare(&store).expect("prepares again");
    assert_eq!(first, second);
}

/// A directory whose parent anyone can write is refused.
///
/// Anyone who can write the parent can replace the directory between the check and the open, or
/// plant a symlink where the store will be created. A store there is not private however careful
/// the code above it is.
#[cfg(unix)]
#[test]
fn a_world_writable_parent_is_refused() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().expect("a temp dir");
    let parent = dir.path().join("open");
    std::fs::create_dir(&parent).expect("creates");
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o777)).expect("chmod");

    let failure = prepare(&parent.join("tessella")).expect_err("the parent is exposed");
    assert!(matches!(failure, StorePathError::Exposed(_)), "{failure:?}");
}

/// A sticky world-writable directory is not an exposure.
///
/// The sticky bit means only an entry's owner may remove or rename it, which is exactly what
/// makes `/tmp` usable. Refusing one would refuse every path under `/tmp` — including the ones
/// these tests run in.
#[cfg(unix)]
#[test]
fn a_sticky_directory_is_allowed() {
    use std::os::unix::fs::PermissionsExt as _;

    let dir = tempfile::tempdir().expect("a temp dir");
    let parent = dir.path().join("sticky");
    std::fs::create_dir(&parent).expect("creates");
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o1777)).expect("chmod");

    prepare(&parent.join("tessella")).expect("a sticky parent is fine");
}

/// A symlinked store directory is refused rather than followed.
#[cfg(unix)]
#[test]
fn a_symlinked_directory_is_refused() {
    let dir = tempfile::tempdir().expect("a temp dir");
    let real = dir.path().join("elsewhere");
    std::fs::create_dir(&real).expect("creates");
    let link = dir.path().join("tessella");
    std::os::unix::fs::symlink(&real, &link).expect("links");

    let failure = prepare(&link).expect_err("a store path is not a link");
    assert!(matches!(failure, StorePathError::Symlink(_)), "{failure:?}");
}

/// A relative path is refused by `prepare` as well as by `from_env`.
///
/// Both doors, because an application may compute a path itself and never call `from_env` at all.
#[test]
fn a_relative_path_is_refused_at_both_doors() {
    let failure = prepare(Path::new("maps/tessella")).expect_err("relative");
    assert!(
        matches!(failure, StorePathError::Relative(_)),
        "{failure:?}"
    );
}

/// The prepared path is one a cache actually opens.
#[cfg(feature = "cache")]
#[test]
fn the_prepared_path_opens_as_a_cache() {
    use tessella_storage::cache::SqliteCache;

    let dir = tempfile::tempdir().expect("a temp dir");
    let file = prepare(&dir.path().join("tessella")).expect("prepares");
    let cache = SqliteCache::open(&file).expect("opens");
    assert!(file.is_file(), "the cache did not create its file");
    drop(cache);
}
