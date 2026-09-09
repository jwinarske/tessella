//! Checks that the built wasm module is what the header says it is.
//!
//! # Why a check on the artifact and not on a compile
//!
//! §19 keeps rediscovering the same thing: `cargo check --target wasm32-unknown-unknown` is a
//! weaker gate than it looks. `std` compiles there and fails at run time, so the workspace passed
//! that lane with a threaded pool and a filesystem store still in it. This is the same hazard one
//! level down. Every lane was green while the module carried 1,380 `__wbindgen_describe_*`
//! exports -- `web-time` reaching `performance` through `js-sys` -- which §19.2 rules out in as
//! many words, and a megabyte of `ureq` behind an entry point that could never connect.
//!
//! Nothing but the module itself says either of those. So this reads the module.
//!
//! ```text
//! cargo run -p wasm-abi -- target/wasm32-unknown-unknown/release/tessella_ffi.wasm
//! ```

use std::path::PathBuf;
use std::process::ExitCode;

/// Every export a consumer is entitled to find.
///
/// `tessella_create` is deliberately not among them: it builds a blocking HTTP source, which a
/// browser has no sockets for, so it is `cfg`'d away on this target. A consumer that wants a map
/// here calls `tessella_create_hosted` and does the fetching.
const REQUIRED: &[&str] = &[
    "memory",
    "tessella_advance",
    "tessella_answer",
    "tessella_create_hosted",
    "tessella_destroy",
    "tessella_fail_request",
    "tessella_pending",
    "tessella_regions",
    "tessella_set_camera",
    "tessella_set_viewport",
    "tessella_set_world_copies",
    "tessella_status",
    "tessella_take_request",
    "tessella_tick",
];

/// How a forbidden name is recognised.
///
/// Two kinds, and the difference is not academic: `tessella_create` is a whole name, and matching
/// it loosely also matches `tessella_create_hosted`, which is required. The first run of this
/// tool failed on exactly that.
enum Match {
    /// The whole export name.
    Whole,
    /// Anything starting with it, for a family of generated names.
    Prefix,
}

/// Exports that must not be there, and what each one means if it is.
const FORBIDDEN: &[(Match, &str, &str)] = &[
    (
        Match::Prefix,
        "__wbindgen",
        "wasm-bindgen reached the producer; §19.2 rules it out, and the usual way in is a \
         dependency that wants a browser API",
    ),
    (
        Match::Whole,
        "tessella_create",
        "the blocking HTTP constructor was built for a target with no sockets",
    ),
];

fn main() -> ExitCode {
    let Some(path) = std::env::args_os().nth(1).map(PathBuf::from) else {
        eprintln!("usage: wasm-abi <module.wasm>");
        return ExitCode::FAILURE;
    };
    let module = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) => {
            eprintln!("wasm-abi: {}: {error}", path.display());
            return ExitCode::FAILURE;
        }
    };

    let exports = match exports(&module) {
        Ok(exports) => exports,
        Err(why) => {
            eprintln!("wasm-abi: {}: {why}", path.display());
            return ExitCode::FAILURE;
        }
    };

    let mut faults = Vec::new();
    for name in REQUIRED {
        if !exports.iter().any(|export| export == name) {
            faults.push(format!("missing export `{name}`"));
        }
    }
    for (kind, name, why) in FORBIDDEN {
        let found = exports
            .iter()
            .filter(|export| match kind {
                Match::Whole => *export == name,
                Match::Prefix => export.starts_with(name),
            })
            .count();
        if found > 0 {
            faults.push(format!("{found} export(s) matching `{name}`: {why}"));
        }
    }

    if faults.is_empty() {
        println!(
            "{}: {} exports, all {} required present",
            path.display(),
            exports.len(),
            REQUIRED.len()
        );
        return ExitCode::SUCCESS;
    }
    for fault in &faults {
        eprintln!("wasm-abi: {fault}");
    }
    ExitCode::FAILURE
}

/// The names in the module's export section.
///
/// Hand-rolled rather than pulled from a parser crate. The export section is a length-prefixed
/// list of length-prefixed names, the rest of the module is skipped by its section header, and a
/// dependency for that would be more code to trust than the forty lines it replaces.
fn exports(module: &[u8]) -> Result<Vec<String>, String> {
    if module.len() < 8 || &module[..4] != b"\0asm" {
        return Err("not a wasm module".into());
    }
    let mut at = 8;
    let mut found = Vec::new();
    while at < module.len() {
        let id = module[at];
        at += 1;
        let (size, next) = leb(module, at)?;
        at = next;
        let end = at.checked_add(size).ok_or("a section runs past the file")?;
        if end > module.len() {
            return Err("a section runs past the file".into());
        }
        // Seven is the export section. Every other section is skipped by its own length, which
        // is what makes this robust against sections this does not know about.
        if id == 7 {
            let mut scan = at;
            let (count, next) = leb(module, scan)?;
            scan = next;
            for _ in 0..count {
                let (len, next) = leb(module, scan)?;
                scan = next;
                let stop = scan
                    .checked_add(len)
                    .ok_or("a name runs past the section")?;
                if stop > end {
                    return Err("a name runs past the section".into());
                }
                found.push(String::from_utf8_lossy(&module[scan..stop]).into_owned());
                // The kind byte, then the index, neither of which this needs.
                scan = stop + 1;
                let (_, next) = leb(module, scan)?;
                scan = next;
            }
        }
        at = end;
    }
    Ok(found)
}

/// One unsigned LEB128, and where it ended.
fn leb(bytes: &[u8], mut at: usize) -> Result<(usize, usize), String> {
    let mut value = 0usize;
    let mut shift = 0u32;
    loop {
        let byte = *bytes.get(at).ok_or("the module ends mid-number")?;
        at += 1;
        value |= usize::from(byte & 0x7f)
            .checked_shl(shift)
            .ok_or("a number too large for this file to contain")?;
        if byte & 0x80 == 0 {
            return Ok((value, at));
        }
        shift += 7;
        if shift > 63 {
            return Err("a number too large for this file to contain".into());
        }
    }
}
