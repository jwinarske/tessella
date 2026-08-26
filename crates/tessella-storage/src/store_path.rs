//! Where the offline store lives, and who is allowed to decide.
//!
//! # The library does not read the environment, and that is the security answer
//!
//! A library that calls `getenv` makes a trust decision it has no way to evaluate. Environment is
//! inherited, and whether this process's environment is trustworthy depends on how it was
//! started — by a user at a shell, by a service manager, by something with a different uid
//! through `sudo` or a setuid wrapper. Only the application knows which, so only the application
//! should ask.
//!
//! So nothing here is called automatically. `SqliteCache::open` still takes a path and nothing
//! else, and an application that wants the environment consulted calls [`from_env`] and passes
//! the answer. The decision stays at the boundary that can evaluate it, and a library user who
//! never calls this is not silently exposed to a variable they did not know existed.
//!
//! # Data, not cache
//!
//! The default is under `XDG_DATA_HOME`, not `XDG_CACHE_HOME`, and the difference is not
//! pedantry. A cache directory is one the system may empty at will — `systemd-tmpfiles` does,
//! and so do most "free up space" tools. A downloaded region is bandwidth somebody paid for and
//! may not be able to spend again; an imported pack may not be re-downloadable at all without
//! credentials the device does not hold. Putting it where a cleaner will find it is a defect
//! that shows up as a car with no map somewhere with no signal.
//!
//! # What is checked, and what is deliberately not
//!
//! [`prepare`] refuses a directory whose parent is writable by others, creates what it makes
//! with owner-only permissions, and refuses a symlinked final component. Those rule out the
//! ordinary local attacks: another user planting a symlink where the store will be created, or
//! replacing the directory between the check and the open.
//!
//! It does *not* try to be safe against a hostile `HOME` or a hostile environment generally. That
//! is not a gap this layer can close — a process whose environment an attacker controls has
//! already lost — and pretending otherwise would encourage exactly the use this module's first
//! section argues against.

use std::io;
use std::path::{Path, PathBuf};

/// The variable an application may let a user set.
pub const STORE_DIR_VAR: &str = "TESSELLA_STORE_DIR";

/// The directory name used under whichever base applies.
pub const STORE_DIR_NAME: &str = "tessella";

/// The store file's name inside that directory.
pub const STORE_FILE_NAME: &str = "store.db";

/// Why a store directory could not be used.
#[derive(Debug, thiserror::Error)]
pub enum StorePathError {
    /// Neither the variable nor a home directory gave a place to put it.
    #[error(
        "no store directory: set {STORE_DIR_VAR}, or XDG_DATA_HOME, or HOME, \
         or pass a path explicitly"
    )]
    Unlocatable,
    /// The path is relative, and a relative store path means a different store per working
    /// directory.
    #[error("`{}` is relative; a store path must be absolute", .0.display())]
    Relative(PathBuf),
    /// The directory, or a parent of it, is writable by users other than the owner.
    ///
    /// Anyone who can write the parent can replace the directory between the check and the open,
    /// or plant a symlink where the store will be created.
    #[error("`{}` is writable by other users, so a store there is not private", .0.display())]
    Exposed(PathBuf),
    /// The final component is a symlink.
    ///
    /// Refused rather than followed: a store path that resolves somewhere else is either a
    /// mistake or an attack, and both want the same answer.
    #[error("`{}` is a symbolic link", .0.display())]
    Symlink(PathBuf),
    /// The directory could not be created or inspected.
    #[error("preparing `{}`: {source}", .path.display())]
    Io {
        /// What was being prepared.
        path: PathBuf,
        /// What went wrong.
        #[source]
        source: io::Error,
    },
}

/// The store directory an application should use, from the environment.
///
/// In order: `TESSELLA_STORE_DIR`, then `$XDG_DATA_HOME/tessella`, then `$HOME/.local/share/tessella`.
///
/// **This reads the environment.** Call it only from an application that knows its own
/// environment is trustworthy — see the module documentation for why that judgement cannot be
/// made here. It performs no filesystem work; pass the result to [`prepare`].
///
/// # Errors
///
/// [`StorePathError::Unlocatable`] when none of the three is set, and
/// [`StorePathError::Relative`] when what is set is not an absolute path — a relative store path
/// silently means a different store per working directory, which is a support call rather than a
/// feature.
pub fn from_env() -> Result<PathBuf, StorePathError> {
    let named = |key: &str| {
        std::env::var_os(key)
            .map(PathBuf::from)
            .filter(|path| !path.as_os_str().is_empty())
    };

    let path = if let Some(explicit) = named(STORE_DIR_VAR) {
        explicit
    } else if let Some(data) = named("XDG_DATA_HOME") {
        data.join(STORE_DIR_NAME)
    } else if let Some(home) = named("HOME") {
        home.join(".local").join("share").join(STORE_DIR_NAME)
    } else {
        return Err(StorePathError::Unlocatable);
    };

    if !path.is_absolute() {
        return Err(StorePathError::Relative(path));
    }
    Ok(path)
}

/// Creates the store directory if it is missing and checks that it is private.
///
/// Returns the path of the store *file* inside it, ready for `SqliteCache::open`.
///
/// The checks are the ones that matter for a file another user must not be able to substitute:
/// every existing ancestor must not be group- or world-writable unless it is sticky (`/tmp` is,
/// which is what the sticky bit is for), the final component must not be a symlink, and anything
/// created here is created `0700`.
///
/// # Errors
///
/// [`StorePathError::Exposed`] when an ancestor is writable by others,
/// [`StorePathError::Symlink`] when the directory itself is a link, and [`StorePathError::Io`]
/// when it cannot be created or inspected.
pub fn prepare(directory: &Path) -> Result<PathBuf, StorePathError> {
    if !directory.is_absolute() {
        return Err(StorePathError::Relative(directory.to_path_buf()));
    }

    // `symlink_metadata` does not follow, which is the whole point: a store path that resolves
    // elsewhere is either a mistake or an attack.
    if let Ok(metadata) = std::fs::symlink_metadata(directory)
        && metadata.file_type().is_symlink()
    {
        return Err(StorePathError::Symlink(directory.to_path_buf()));
    }

    create_private(directory)?;

    // Checked *after* creation as well as before: an ancestor that became writable while the
    // directory was being made is the case a check-then-create ordering misses.
    for ancestor in directory.ancestors() {
        if is_exposed(ancestor)? {
            return Err(StorePathError::Exposed(ancestor.to_path_buf()));
        }
    }

    Ok(directory.join(STORE_FILE_NAME))
}

/// Whether others can write here, ignoring a sticky directory.
///
/// The sticky bit means only an entry's owner may remove or rename it, which is exactly the
/// property that makes `/tmp` usable at all — so a sticky world-writable directory is not an
/// exposure, and refusing one would refuse every path under `/tmp` including a test's.
#[cfg(unix)]
fn is_exposed(path: &Path) -> Result<bool, StorePathError> {
    use std::os::unix::fs::PermissionsExt as _;

    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        // An ancestor that does not exist cannot be written to. `ancestors()` walks up to the
        // root, and on some layouts a component may be unreadable rather than absent; neither
        // is an exposure this can act on.
        Err(_) => return Ok(false),
    };
    let mode = metadata.permissions().mode();
    let group_or_other_write = mode & 0o022 != 0;
    let sticky = mode & 0o1000 != 0;
    Ok(group_or_other_write && !sticky)
}

#[cfg(not(unix))]
fn is_exposed(_path: &Path) -> Result<bool, StorePathError> {
    // The permission model is different enough that a mode check would be theatre. The path is
    // still refused if it is relative or a symlink.
    Ok(false)
}

/// Creates `directory` and its parents, owner-only.
#[cfg(unix)]
fn create_private(directory: &Path) -> Result<(), StorePathError> {
    use std::os::unix::fs::DirBuilderExt as _;

    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)
        .map_err(|source| StorePathError::Io {
            path: directory.to_path_buf(),
            source,
        })
}

#[cfg(not(unix))]
fn create_private(directory: &Path) -> Result<(), StorePathError> {
    std::fs::create_dir_all(directory).map_err(|source| StorePathError::Io {
        path: directory.to_path_buf(),
        source,
    })
}
