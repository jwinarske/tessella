//! `include/tessella.h` compiled, linked and run as a consumer would.
//!
//! # Why this exists
//!
//! The capture ABI header is generated (DR-6) because it is a large table nobody could keep in
//! step by reading it. This one is six functions and two structs, and generating it would cost
//! more than it saves — but a hand-written header drifts, and a drifted header is discovered by
//! whoever integrates against it rather than by whoever changed the Rust.
//!
//! So the header is checked instead of trusted. The probe sees only the declarations: a signature
//! that disagrees fails to compile or fails to link, a struct whose layout disagrees fails its
//! static assertion, and behaviour that disagrees fails an assertion here. It is C rather than
//! C++ on purpose — the header claims to be a C surface, and a C++ compiler accepts things C does
//! not.

use std::path::PathBuf;
use std::process::Command;

/// Where cargo left the staticlib.
///
/// From the test binary rather than from a guessed profile name: this file has no idea whether it
/// is running under `debug`, `release` or a custom profile, and the binary does.
fn profile_dir() -> PathBuf {
    let exe = std::env::current_exe().expect("the test binary has a path");
    exe.parent()
        .and_then(std::path::Path::parent)
        .expect("target/<profile>/deps/<test>")
        .to_path_buf()
}

#[test]
fn the_c_header_describes_the_library_it_claims_to() {
    let root = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../.."));
    let source = root.join("crates/tessella-ffi/tests/c_surface.c");
    let staticlib = profile_dir().join("libtessella_ffi.a");

    // `cargo test` builds the rlib the harness links against; the staticlib is a separate
    // artefact of the same crate. Skipping rather than failing when it is absent keeps
    // `cargo test -p tessella-ffi --lib` usable on its own, and CI builds the workspace first.
    if !staticlib.exists() {
        eprintln!(
            "skipping: {} not built — run `cargo build -p tessella-ffi` first",
            staticlib.display()
        );
        return;
    }

    let dir = std::env::temp_dir().join(format!("tessella-c-surface-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a scratch directory");
    let out = dir.join("c_surface");

    let compiler = std::env::var("CC").unwrap_or_else(|_| "cc".to_string());
    let done = Command::new(compiler)
        .arg("-std=c11")
        // The header is the contract, so anything it makes a compiler complain about is a defect
        // in the header rather than noise to be turned down.
        .arg("-Wall")
        .arg("-Wextra")
        .arg("-Werror")
        .arg("-I")
        .arg(root.join("include"))
        .arg("-o")
        .arg(&out)
        .arg(&source)
        .arg(&staticlib)
        // What a Rust staticlib needs from the platform. A consumer linking this into its own
        // shared object needs the same list, which is the other reason to have it written down
        // somewhere that is executed rather than in prose.
        .arg("-lpthread")
        .arg("-ldl")
        .arg("-lm")
        .output()
        .expect("the compiler runs");

    assert!(
        done.status.success(),
        "the C surface did not build against its own header:\n{}",
        String::from_utf8_lossy(&done.stderr)
    );

    let run = Command::new(&out).output().expect("the probe runs");
    std::fs::remove_dir_all(&dir).ok();
    assert!(
        run.status.success(),
        "the probe exited {}:\n{}",
        run.status,
        String::from_utf8_lossy(&run.stderr)
    );

    let said: std::collections::BTreeMap<String, i64> = String::from_utf8_lossy(&run.stdout)
        .lines()
        .filter_map(|line| {
            let (name, value) = line.split_once(' ')?;
            Some((name.to_owned(), value.parse().ok()?))
        })
        .collect();

    let check = |name: &str, want: i64, why: &str| {
        assert_eq!(
            said.get(name).copied(),
            Some(want),
            "{name}: {why} ({said:?})"
        );
    };

    // TESSELLA_OK is 0, and every one of these is a call that must succeed.
    check("create", 0, "a valid style did not create");
    check("handle_non_null", 1, "create returned OK without a handle");
    check("set_camera", 0, "the camera did not move");
    check("tick_first", 0, "the first tick failed");
    check("tick_second", 0, "the second tick failed");
    check("status", 0, "the status call failed");
    check("status_no_reason", 0, "status refused a null reason buffer");
    check("regions", 0, "the regions call failed");
    check("done", 1, "the probe did not reach the end");

    // TESSELLA_BAD_STYLE is 4, and a failed create must not hand back a handle -- a caller that
    // ignores the status still cannot mistake one for a working map.
    check("bad_style", 4, "a style that cannot parse was accepted");
    check("bad_style_handle_null", 1, "a failed create wrote a handle");

    // TESSELLA_NULL_ARGUMENT is 1; TESSELLA_NO_SUCH_MAP is 2. A null handle is the second of
    // those rather than the first: the argument was supplied, it just does not name a live map.
    check("null_config", 1, "a null config was not rejected");
    check("null_out", 1, "a null out-pointer was not rejected");
    check("null_map_tick", 2, "ticking a null handle was not rejected");

    // This style has no sources, so nothing can resolve and nothing needs to: the map is ready
    // as soon as it is asked, and there is no failure to report.
    check(
        "readiness",
        2,
        "a sourceless map did not reach TESSELLA_READY",
    );
    check(
        "readiness_again",
        2,
        "the second status disagreed with the first",
    );
    check(
        "reason_empty",
        1,
        "a reason was written for a map that did not fail",
    );

    // The ring is the whole point of the arrangement: a consumer reads it directly.
    check("ring_non_null", 1, "the ring range was null");
    check("ring_len_nonzero", 1, "the ring range was empty");
}
