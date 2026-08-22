# tessella

A Rust frontend for the MapLibre style spec, emitting a renderer-agnostic capture stream.

**Status: name-reservation stub (0.0.0).** No API yet. This crate will become the facade
re-exporting the `tessella-*` workspace crates — style parse and expression evaluation,
source and tile management, layout, placement, and the render orchestrator that produces
the capture stream. The renderer lives on the other side of that stream and is not part of
this project.

Development happens at <https://github.com/jwinarske/tessella>.

Independent of the MapLibre organization and its trademarks; "MapLibre style spec" names
the specification this frontend implements.

Licensed under BSD-2-Clause.
