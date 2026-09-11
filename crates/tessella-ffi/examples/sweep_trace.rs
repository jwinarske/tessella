//! The native half of the browser sweep's determinism gate.
//!
//! ```sh
//! TESSELLA_WORKERS=0 cargo run -p tessella-ffi --release --example sweep_trace -- [--steps N]
//! ```
//!
//! Drives four hosted maps through the four-view sweep exactly as `web/bench.worker.js` does under
//! its fixed clock, over the same style and the same tile, and prints a fingerprint of each map's
//! frame: FNV-1a 64 over every record, another over the slab bytes its geometry names, and the
//! camera records whole. The browser prints the same lines, and `web/test/bench.mjs` holds the two
//! to identical hashes and to cameras that differ only in the last bits of their libm-derived
//! doubles -- `web/bench/trace.js` says why the camera is the exception.
//!
//! # Serial, or it is not a trace
//!
//! `TESSELLA_WORKERS=0` is the pool's deterministic mode: no worker threads, every job run by the
//! ticking thread inside `tessella_tick`. That is the only arrangement the wasm module has, and a
//! threaded pool lands its builds in whichever frame they finish in. This refuses to run under
//! any other, because a trace taken with workers would diff against the browser for reasons that
//! have nothing to do with the browser.
//!
//! # The frame, as the worker has it
//!
//! Answer what the previous frame asked for, in the order it was asked; advance each map by the
//! fixed step; set each camera; tick each; drain each ring and fingerprint it; read `pending`;
//! take each map's requests. The browser fetches between the last step and the next frame's
//! first, and waits for all of it; answering here from the fixture at the same point is the same
//! sequence of calls.
//!
//! The first line names the views and zooms, from `sweep.rs` itself, so the harness can hold the
//! JavaScript port to the oracle rather than trusting it.

use std::process::ExitCode;
use std::time::Instant;

use tessella_capture_abi::EnvelopeKind;
use tessella_capture_abi::envelope::{AttributeDesc, GeometryAdd, SlabRef};
use tessella_capture_abi::ring::{self, Consumer, RingControl};
use tessella_ffi::{
    Config, MapHandle, Regions, Status, tessella_advance, tessella_answer, tessella_create_hosted,
    tessella_destroy, tessella_pending, tessella_regions, tessella_set_camera,
    tessella_take_request, tessella_tick,
};
use tessella_orchestrate::pool::Pool;
use tessella_orchestrate::sweep;

const STYLE: &str = include_str!("../../../web/bench/style.json");
const TILE: &[u8] = include_bytes!("../../../tests/mvt-fixtures/protomaps-berlin-14-8802-5373.mvt");

/// The slab table's header and one entry, as `ring.js` reads them.
const SLAB_REGION_SIZE: usize = 16;
const SLAB_ENTRY_SIZE: usize = 16;

struct Options {
    steps: usize,
    tick_ms: f64,
    width: u32,
    height: u32,
    ring_bytes: usize,
    max_settle: usize,
    /// Frames whose records are printed one by one, for finding what a diff is about.
    records: Records,
}

#[derive(Clone, Copy, PartialEq)]
enum Records {
    None,
    Frame(usize),
    All,
}

fn options() -> Result<Options, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let value = |name: &str| -> Option<&str> {
        args.iter()
            .position(|arg| arg == name)
            .and_then(|at| args.get(at + 1))
            .map(String::as_str)
    };
    let parse = |name: &str, default: f64| -> Result<f64, String> {
        value(name).map_or(Ok(default), |text| {
            text.parse()
                .map_err(|_| format!("{name}: not a number: {text}"))
        })
    };
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    Ok(Options {
        steps: parse("--steps", 33.0)? as usize,
        tick_ms: parse("--tick-ms", 16.667)?,
        width: parse("--width", 640.0)? as u32,
        height: parse("--height", 480.0)? as u32,
        ring_bytes: parse("--ring", f64::from(4u32 << 20))? as usize,
        max_settle: parse("--max-settle", 600.0)? as usize,
        records: match value("--records") {
            None => Records::None,
            Some("all") => Records::All,
            Some(text) => Records::Frame(
                text.parse()
                    .map_err(|_| format!("--records: not a frame: {text}"))?,
            ),
        },
    })
}

/// FNV-1a, 64-bit.
struct Fnv(u64);

impl Fnv {
    fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    fn bytes(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            self.0 ^= u64::from(byte);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    fn u32(&mut self, value: u32) {
        self.bytes(&value.to_le_bytes());
    }
}

/// One map, and the consumer's hold on its two regions.
struct Held {
    map: MapHandle,
    consumer: Consumer,
    slabs: *const u8,
    slabs_len: usize,
}

impl Held {
    /// The bytes a slab reference names, or `None` where it names nothing -- the checks
    /// `Slabs.resolve` makes, in its order.
    fn resolve(&self, reference: SlabRef) -> Option<&[u8]> {
        // SAFETY: the slab region is `slabs_len` bytes owned by the live map, and is not written
        // between a tick and the next one, which is the only window this is called in.
        let region = unsafe { core::slice::from_raw_parts(self.slabs, self.slabs_len) };
        if region.len() < SLAB_REGION_SIZE {
            return None;
        }
        let word = |at: usize| u32::from_le_bytes(region[at..at + 4].try_into().expect("four"));
        let long = |at: usize| u64::from_le_bytes(region[at..at + 8].try_into().expect("eight"));
        if word(0) != tessella_capture_abi::ABI_REV {
            return None;
        }
        if reference.slab >= word(4) {
            return None;
        }
        let entry = SLAB_REGION_SIZE + reference.slab as usize * SLAB_ENTRY_SIZE;
        if entry + SLAB_ENTRY_SIZE > self.slabs_len {
            return None;
        }
        let base = usize::try_from(long(entry)).ok()?;
        let len = usize::try_from(long(entry + 8)).ok()?;
        let (offset, length) = (reference.offset as usize, reference.length as usize);
        let end = offset.checked_add(length)?;
        if end > len {
            return None;
        }
        region.get(base.checked_add(offset)?..base.checked_add(end)?)
    }

    /// Drains the ring and fingerprints what was on it, printing each record when `show` names
    /// the frame and map.
    fn fingerprint(&mut self, show: Option<(usize, usize)>) -> Print {
        let mut stream = Fnv::new();
        let mut slabs = Fnv::new();
        let mut cameras = Vec::new();
        let mut records = 0;
        // Geometry is fingerprinted after the drain, since `resolve` borrows the map while the
        // record borrows the ring.
        let mut geometry: Vec<(GeometryAdd, Vec<AttributeDesc>)> = Vec::new();
        while let Some(record) = self.consumer.peek() {
            records += 1;
            if let Some((frame, map)) = show {
                let mut one = Fnv::new();
                one.bytes(record.record);
                one.bytes(record.payload);
                let hex: String = record.record.iter().map(|b| format!("{b:02x}")).collect();
                println!(
                    "RECORD {frame} {map} {} {} {} {:016x} {hex}",
                    kind_name(record.kind),
                    record.record.len(),
                    record.payload.len(),
                    one.0
                );
            }
            stream.bytes(&(record.kind as u16).to_le_bytes());
            stream.u32(u32::try_from(record.record.len()).expect("a record fits a u32"));
            // The camera is carried whole rather than hashed: its doubles come from the
            // platform's libm, which wasm's does not match in the last bits. `trace.js` says why.
            if record.kind == EnvelopeKind::CameraUpdate {
                cameras.push(
                    record
                        .record
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect::<String>(),
                );
            } else {
                stream.bytes(record.record);
            }
            stream.u32(u32::try_from(record.payload.len()).expect("a payload fits a u32"));
            stream.bytes(record.payload);
            if record.kind == EnvelopeKind::GeometryAdd
                && record.record.len() >= size_of::<GeometryAdd>()
            {
                // SAFETY: at least a `GeometryAdd`'s bytes, read unaligned; it is plain data.
                let add = unsafe {
                    record
                        .record
                        .as_ptr()
                        .cast::<GeometryAdd>()
                        .read_unaligned()
                };
                // A span that runs past the payload ends the list where the payload does, and the
                // slab hash then differs from the side that read it whole, which is the report.
                let attrs = (0..add.attrs.count as usize)
                    .map_while(|i| {
                        let at = add.attrs.offset as usize + i * size_of::<AttributeDesc>();
                        let bytes = record.payload.get(at..at + size_of::<AttributeDesc>())?;
                        // SAFETY: an `AttributeDesc`'s bytes, read unaligned; it is plain data.
                        Some(unsafe { bytes.as_ptr().cast::<AttributeDesc>().read_unaligned() })
                    })
                    .collect();
                geometry.push((add, attrs));
            }
            let consumed = record.consumed();
            self.consumer.advance(consumed);
        }
        for (add, attrs) in &geometry {
            for reference in core::iter::once(add.indexes).chain(attrs.iter().map(|a| a.source)) {
                match self.resolve(reference) {
                    Some(bytes) => {
                        slabs.u32(u32::try_from(bytes.len()).expect("a slab fits a u32"));
                        slabs.bytes(bytes);
                    }
                    None => slabs.u32(u32::MAX),
                }
            }
        }
        Print {
            records,
            hash: stream.0,
            slabs: slabs.0,
            cameras,
        }
    }
}

/// One map's frame, fingerprinted as `trace.js` fingerprints it.
struct Print {
    records: usize,
    hash: u64,
    slabs: u64,
    cameras: Vec<String>,
}

/// A kind as `abi.js` spells it, so the two sides' record listings diff line for line.
fn kind_name(kind: EnvelopeKind) -> String {
    let mut out = String::new();
    for (i, c) in format!("{kind:?}").chars().enumerate() {
        if c.is_uppercase() && i > 0 {
            out.push('_');
        }
        out.push(c.to_ascii_uppercase());
    }
    out
}

fn status(code: Status) -> i32 {
    code as i32
}

fn main() -> ExitCode {
    let options = match options() {
        Ok(options) => options,
        Err(message) => {
            eprintln!("sweep_trace: {message}");
            return ExitCode::FAILURE;
        }
    };
    if Pool::shared().workers() != 0 {
        eprintln!(
            "sweep_trace: the pool has {} workers; run with TESSELLA_WORKERS=0",
            Pool::shared().workers()
        );
        return ExitCode::FAILURE;
    }

    let views = sweep::four_views();
    let zooms = sweep::sweep_zooms(options.steps);
    let described: Vec<String> = views
        .iter()
        .map(|v| {
            format!(
                r#"{{"latitude":{},"longitude":{}}}"#,
                v.latitude, v.longitude
            )
        })
        .collect();
    let zoom_list: Vec<String> = zooms.iter().map(|z| format!("{z:?}")).collect();
    println!(
        r#"ORACLE {{"views":[{}],"zooms":[{}],"low":{:?}}}"#,
        described.join(","),
        zoom_list.join(","),
        sweep::SWEEP_LOW
    );

    let config = Config {
        style_json: STYLE.as_ptr(),
        style_json_len: STYLE.len(),
        width: options.width,
        height: options.height,
        ring_capacity: options.ring_bytes,
        slab_capacity: 0,
    };
    let mut held: Vec<Held> = Vec::new();
    for view in &views {
        let mut map: MapHandle = core::ptr::null_mut();
        // SAFETY: the config and the style it points at outlive the call, which copies the style.
        let created = unsafe {
            tessella_create_hosted(
                &config,
                view.latitude,
                view.longitude,
                sweep::SWEEP_LOW,
                &mut map,
            )
        };
        if created != Status::Ok || map.is_null() {
            eprintln!("sweep_trace: tessella_create_hosted answered {created:?}");
            return ExitCode::FAILURE;
        }
        let mut regions = Regions {
            ring: core::ptr::null(),
            ring_len: 0,
            slabs: core::ptr::null(),
            slabs_len: 0,
        };
        // SAFETY: a live map and a valid out pointer.
        if unsafe { tessella_regions(map, &mut regions) } != Status::Ok {
            eprintln!("sweep_trace: tessella_regions refused");
            return ExitCode::FAILURE;
        }
        // SAFETY: the ring region begins with an initialized control block, eight-aligned, and
        // lives as long as the map.
        let capacity = unsafe { (*regions.ring.cast::<RingControl>()).capacity } as usize;
        // SAFETY: as above; the producer half this also returns is dropped unused.
        let Some((_, consumer)) = (unsafe { ring::attach(regions.ring.cast_mut(), capacity) })
        else {
            eprintln!("sweep_trace: the ring did not attach");
            return ExitCode::FAILURE;
        };
        held.push(Held {
            map,
            consumer,
            slabs: regions.slabs,
            slabs_len: regions.slabs_len,
        });
    }

    let mut queue: Vec<(usize, u64)> = Vec::new();
    let mut frame = |index: usize, phase: &str, zoom: f64, held: &mut Vec<Held>| -> (usize, bool) {
        for (map, ticket) in queue.drain(..) {
            // SAFETY: a live map, and the fixture is valid for its length.
            unsafe { tessella_answer(held[map].map, ticket, 200, TILE.as_ptr(), TILE.len()) };
        }
        for h in held.iter() {
            // SAFETY: a live map.
            unsafe { tessella_advance(h.map, options.tick_ms) };
        }
        for (h, view) in held.iter().zip(&views) {
            // SAFETY: a live map.
            unsafe { tessella_set_camera(h.map, view.latitude, view.longitude, zoom, 0.0, 0.0) };
        }
        let mut tick_ms = Vec::with_capacity(held.len());
        let statuses: Vec<i32> = held
            .iter()
            .map(|h| {
                let started = Instant::now();
                // SAFETY: a live map.
                let code = status(unsafe { tessella_tick(h.map) });
                tick_ms.push(format!("{:.3}", started.elapsed().as_secs_f64() * 1000.0));
                code
            })
            .collect();
        let show = options.records == Records::All || options.records == Records::Frame(index);
        let prints: Vec<Print> = held
            .iter_mut()
            .enumerate()
            .map(|(map, h)| h.fingerprint(show.then_some((index, map))))
            .collect();
        let pending: Vec<u64> = held
            .iter()
            .map(|h| {
                let mut out = 0u64;
                // SAFETY: a live map and a valid out pointer.
                unsafe { tessella_pending(h.map, &mut out) };
                out
            })
            .collect();
        for (index, h) in held.iter().enumerate() {
            loop {
                let (mut ticket, mut url, mut len) = (0u64, core::ptr::null::<u8>(), 0usize);
                // SAFETY: a live map and valid out pointers.
                unsafe { tessella_take_request(h.map, &mut ticket, &mut url, &mut len) };
                if ticket == 0 {
                    break;
                }
                queue.push((index, ticket));
            }
        }

        let maps: Vec<String> = (0..held.len())
            .map(|i| {
                let print = &prints[i];
                let cameras: Vec<String> = print.cameras.iter().map(|c| format!("\"{c}\"")).collect();
                format!(
                    r#"{{"status":{},"pending":{},"records":{},"hash":"{:016x}","slabs":"{:016x}","cameras":[{}]}}"#,
                    statuses[i],
                    pending[i],
                    print.records,
                    print.hash,
                    print.slabs,
                    cameras.join(",")
                )
            })
            .collect();
        // The timings ride along for the harness's native column and are no part of the diff.
        println!(
            r#"TRACE {{"frame":{index},"phase":"{phase}","maps":[{}],"tick_ms":[{}]}}"#,
            maps.join(","),
            tick_ms.join(",")
        );
        let records: usize = prints.iter().map(|p| p.records).sum();
        (records, pending.iter().all(|&p| p == 0))
    };

    let mut index = 0;
    let mut settled = false;
    while index < options.max_settle {
        let (records, quiet) = frame(index, "settle", sweep::SWEEP_LOW, &mut held);
        index += 1;
        if records == 0 && quiet {
            settled = true;
            break;
        }
    }
    if !settled {
        eprintln!(
            "sweep_trace: the four maps did not settle within {} frames",
            options.max_settle
        );
        return ExitCode::FAILURE;
    }
    for &zoom in &zooms {
        frame(index, "sweep", zoom, &mut held);
        index += 1;
    }

    for h in held {
        // SAFETY: each map destroyed once, after its last use.
        unsafe { tessella_destroy(h.map) };
    }
    ExitCode::SUCCESS
}
