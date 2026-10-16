# Sprite fixtures

`emerald.png` and `emerald.json` are maplibre-native's own, from
`test/fixtures/annotations/`. They are the pair its `Sprite.SpriteParsing` test reads, so the
index parsed here is the index mbgl parses from the same bytes — which is what makes the
comparison a comparison rather than two implementations agreeing about a file each invented.

A 200x299 sheet with seventy-three icons in it: markers, shields, patterns, and the
`dlr.london-overground.london-underground.national-rail` family whose names carry dots. Hand-made
fixtures do not have that shape, and a parser that split names on a separator would pass every
test written against one and fail here.

Vendored rather than read from a checkout for the reason `tests/glyph-fixtures` is: the tests run
without maplibre-native present, and the bytes are what is being agreed about.

`stretch.png`/`stretch.json` and `textFit.png`/`textFit.json` are maplibre-native's too, from
`metrics/integration/sprites/`, and they are the only sprites here that name stretch zones.
`stretch` holds the seven shapes its `icon-text-fit` render tests draw — nine parts, fifteen
parts, three, two, and the retina and content-box variants of the first — and `textFit` holds
the nine combinations of `textFitWidth` and `textFitHeight`. `tools/parity/scenes/stretch_p.json`
and `textfit_p.json` draw three of each against the oracle.

`stretch.png` is a two-bit palette image and `emerald.png` is eight-bit RGBA, so the pair is also
the only coverage this tree has of a sub-byte palette reaching the atlas. `stretch_p` reads zero,
which is what says the widening is right: a decoder that mishandled the depth would draw the
wrong colors in the right places and the scene is the only thing that looks.
