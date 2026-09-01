//! The Filament resource path, compiled ahead and loaded headlessly.
//!
//! # What this is proving
//!
//! §16 records three ways to get materials onto the GPU and this is the first of them: compile
//! per-style ahead with `matc`, load the package at run time. What could go wrong is not the
//! plan but the mechanics — whether a `.mat` written against tessella's own blocks compiles at
//! all, whether Filament accepts the package, and whether it takes the buffers a drawable
//! actually arrives as. All three are answered here rather than argued.
//!
//! # Why it needs no GPU
//!
//! `Backend::NOOP` is Filament's no-op driver, and everything this exercises — material
//! compilation, package loading, instancing, vertex and index buffers, renderable construction —
//! happens above the driver. The pixels are the part that needs a device, and the pixels are the
//! part a device would have to judge anyway.
//!
//! # Why it skips rather than fails
//!
//! Filament and `matc` are a large external build that is not part of this workspace. Where they
//! are is a property of the machine, so `TESSELLA_FILAMENT_DIR` names the staging directory and
//! the test says what it wanted when it cannot find it.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Filament's staging directory: `include/`, `lib/<arch>/`, and `matc` beside them.
fn filament_dir() -> PathBuf {
    std::env::var("TESSELLA_FILAMENT_DIR").map_or_else(
        |_| PathBuf::from("/mnt/dev/filament-1.75.0/build/release/staging"),
        PathBuf::from,
    )
}

/// `matc` lives in the build tree rather than in staging, one level up from it.
fn matc(staging: &Path) -> Option<PathBuf> {
    let candidates = [
        staging.join("bin/matc"),
        staging
            .parent()
            .map_or_else(|| PathBuf::from("."), |p| p.join("tools/matc/matc")),
    ];
    candidates.into_iter().find(|path| path.is_file())
}

#[test]
fn a_material_compiled_ahead_loads_and_takes_a_drawables_buffers() {
    let consumer = PathBuf::from(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../tessella_fluorite"
    ));
    let staging = filament_dir();
    let include = staging.join("include");
    let libs = staging.join("lib/x86_64");

    if !consumer.join("materials/fill.mat").exists() {
        eprintln!("skipping: no consumer checkout at {}", consumer.display());
        return;
    }
    if !include.is_dir() || !libs.is_dir() {
        eprintln!(
            "skipping: no Filament staging at {} — set TESSELLA_FILAMENT_DIR",
            staging.display()
        );
        return;
    }
    let Some(matc) = matc(&staging) else {
        eprintln!("skipping: no matc under {}", staging.display());
        return;
    };

    let dir = std::env::temp_dir().join(format!("tessella-filament-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a scratch directory");

    // Every material the consumer carries, so a new one that does not compile fails here rather
    // than on the device.
    let mut packages = Vec::new();
    for entry in std::fs::read_dir(consumer.join("materials")).expect("materials are readable") {
        let source = entry.expect("a directory entry").path();
        if source.extension().is_none_or(|ext| ext != "mat") {
            continue;
        }
        let name = source
            .file_stem()
            .expect("a .mat has a stem")
            .to_string_lossy()
            .into_owned();
        let package = dir.join(format!("{name}.filamat"));
        let done = Command::new(&matc)
            // Vulkan and desktop, which is DR-16's target and what fluorite runs.
            .arg("-a")
            .arg("vulkan")
            .arg("-p")
            .arg("desktop")
            .arg("-o")
            .arg(&package)
            .arg(&source)
            .output()
            .expect("matc runs");
        assert!(
            done.status.success(),
            "{name}.mat did not compile:\n{}",
            String::from_utf8_lossy(&done.stderr)
        );
        packages.push((name, package));
    }
    assert!(!packages.is_empty(), "no materials were found to compile");

    let archives: Vec<PathBuf> = std::fs::read_dir(&libs)
        .expect("the lib directory is readable")
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            (path.extension()? == "a").then_some(path)
        })
        .collect();

    let out = dir.join("filament_probe");
    let mut build = Command::new("clang++");
    build
        .arg("-std=c++17")
        .arg("-stdlib=libc++")
        .arg("-I")
        .arg(&include)
        .arg("-o")
        .arg(&out)
        .arg(consumer.join("native/test/filament_probe.cc"))
        .arg("-Wl,--start-group");
    for archive in &archives {
        build.arg(archive);
    }
    // EGL and GL because Filament's backend archive carries its EGL platform whichever driver is
    // selected -- the NOOP driver never calls them, but the objects are in the archive and the
    // linker still wants their symbols.
    build
        .arg("-Wl,--end-group")
        .arg("-lEGL")
        .arg("-lGL")
        .arg("-lpthread")
        .arg("-ldl")
        .arg("-lm");
    let built = build.output().expect("clang++ runs");
    assert!(
        built.status.success(),
        "the Filament probe did not link:\n{}",
        String::from_utf8_lossy(&built.stderr)
    );

    for (name, package) in &packages {
        let run = Command::new(&out).arg(package).output().expect("it runs");
        let stderr = String::from_utf8_lossy(&run.stderr).into_owned();
        assert!(
            run.status.success(),
            "the probe exited {} on {name}:\n{stderr}",
            run.status
        );

        let said: std::collections::BTreeMap<String, i64> = String::from_utf8_lossy(&run.stdout)
            .lines()
            .filter_map(|line| {
                let (key, value) = line.split_once(' ')?;
                Some((key.to_owned(), value.parse().ok()?))
            })
            .collect();
        let got = |what: &str| said.get(what).copied().unwrap_or(-1);

        assert!(
            got("package_bytes") > 0,
            "{name} compiled to nothing: {said:?}"
        );
        assert_eq!(got("engine"), 1, "no NOOP engine for {name}: {said:?}");
        assert_eq!(
            got("material"),
            1,
            "Filament would not load {name}'s package: {said:?}"
        );
        assert_eq!(got("instance"), 1, "{name} would not instance: {said:?}");

        // The buffers a fill drawable actually arrives as: tile-local i16 positions and u16
        // indices. A material that cannot take those is one no drawable can use.
        assert_eq!(
            got("vertex_buffer"),
            1,
            "{name} rejected SHORT2 tile-local positions: {said:?}"
        );
        assert_eq!(
            got("index_buffer"),
            1,
            "{name} rejected USHORT indices: {said:?}"
        );
        assert_eq!(
            got("renderable"),
            1,
            "{name} did not become a renderable: {said:?}"
        );
        assert_eq!(
            got("scene_entities"),
            1,
            "{name}'s renderable did not reach the scene: {said:?}"
        );
        assert_eq!(got("done"), 1, "the probe did not finish {name}: {said:?}");
    }

    std::fs::remove_dir_all(&dir).ok();
}
