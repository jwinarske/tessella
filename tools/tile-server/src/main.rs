//! Serves a directory of tiles, or one fixture at every tile, over plain HTTP.
//!
//! ```text
//! tile-server <tile.mvt> [--port N] [--minzoom N] [--maxzoom N]
//! ```
//!
//! Prints the TileJSON URL it is serving, so a style can be pointed at it directly. Runs until
//! interrupted.

use std::io::Write as _;

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(fixture) = args.first().filter(|arg| !arg.starts_with('-')) else {
        eprintln!("usage: tile-server <tile.mvt> [--minzoom N] [--maxzoom N]");
        return std::process::ExitCode::FAILURE;
    };

    let flag = |name: &str, default: u8| -> u8 {
        args.iter()
            .position(|arg| arg == name)
            .and_then(|index| args.get(index + 1))
            .and_then(|value| value.parse().ok())
            .unwrap_or(default)
    };
    let (minzoom, maxzoom) = (flag("--minzoom", 0), flag("--maxzoom", 14));

    let body = match tile_server::read(fixture) {
        Ok(body) => body,
        Err(error) => {
            eprintln!("tile-server: {fixture}: {error}");
            return std::process::ExitCode::FAILURE;
        }
    };

    // The TileJSON has to name the origin, which is not known until the port is bound — so the
    // server starts with the tile route alone and is restarted with the manifest once its
    // address is known. Two binds rather than a mutable route table, because the table is
    // shared with the serving thread and this runs once at startup.
    let probe = match tile_server::Server::start(tile_server::Routes::new()) {
        Ok(server) => server,
        Err(error) => {
            eprintln!("tile-server: bind: {error}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let origin = probe.origin();
    drop(probe);

    let tilejson = format!(
        r#"{{"tilejson":"3.0.0","tiles":["{origin}/{{z}}/{{x}}/{{y}}.pbf"],"minzoom":{minzoom},"maxzoom":{maxzoom}}}"#
    );
    let routes = tile_server::Routes::new()
        .at("/tiles.json", "application/json", tilejson.into_bytes())
        .tiles(body, Some((minzoom, maxzoom)));

    let server = match tile_server::Server::start(routes) {
        Ok(server) => server,
        Err(error) => {
            eprintln!("tile-server: bind: {error}");
            return std::process::ExitCode::FAILURE;
        }
    };

    // The re-bind is not guaranteed to land on the same port; report the one actually bound.
    println!("tile-server: {}/tiles.json", server.origin());
    println!(
        "tile-server: tiles at {}/{{z}}/{{x}}/{{y}}.pbf",
        server.origin()
    );
    let _ = std::io::stdout().flush();

    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
