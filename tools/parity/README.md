# Visual parity

The §9.1 gate: the same camera through maplibre-native's own renderer and through this one,
counted in gross pixels. A change that moves these numbers has changed what the map looks like,
whether or not it meant to.

## Running it

    tools/parity/build.sh        # render_probe and the materials, from the current trees
    tools/parity/sweep.sh        # the five cameras, against the oracle

Needs three things the tree does not carry: maplibre-native's `mbgl-render` as the oracle, a
Filament build, and the tile and asset servers the scenes name (`serve.sh` on 8080 and
`assets.py` on 8081). `env.sh` says where each is expected and every path is an override.

## What the numbers mean

`gross N of M` counts pixels whose per-channel difference exceeds 48. Not a mean: a mean hides a
hundred wrong pixels among a million right ones, and the question is how many pixels a person
would call different. Pass 12 as a third argument to `gross.py` for the second lens, which is
what to use when a change is supposed to move nothing at all.

The sweep's numbers as of 2026-09-13, which is the gate:

    families_p  z14 p0    24 of 786432
    families_p  z14 p60   45
    families_p  z16 p0     5
    families_p  z16 p60   52
    families_p  z9  p0    30 of 2160000

## The scenes

- `families_p` — one layer of every family this build draws, over Berlin. The sweep's scene.
- `quad_remote` — the four-pane quad's style, read over https with nothing on disk. Used by the
  consumer's own `quad_probe` rather than by `sweep.sh`.
- `heat_p` — a heatmap over Berlin's POIs. **This one does not pass and is not meant to yet**:
  the layer is not drawn, so the run reports the distance to go. It was captured before any
  heatmap code existed so the design behind it has a referee rather than an argument. At
  `f3054a1` it stood at `gross 271571 of 786432 (34.532%)`, against an oracle whose heatmap
  covers 48.7% of the frame.

## Why the scenes are here and the frames are not

A scene is a question and belongs with the code it asks about. A rendered frame is an answer to
one run of it, is megabytes, and is regenerated in seconds — including the oracle's, which
`parity.sh` re-renders every time rather than trusting a stored PNG whose camera nobody wrote
down. Everything a run produces goes to `PARITY_WORK`, outside the tree.

No tile or glyph data is vendored here. The scenes name servers; what those serve stays where it
is licensed.
