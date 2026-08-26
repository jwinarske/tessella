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
