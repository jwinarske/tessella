"""Mechanically convert mbgl's update_renderables.test.cpp expectations into Rust.

Hand-transcribing 1400 lines of expectations would introduce exactly the class of quiet error
the tests exist to catch, so the conversion is done by rule and the rules are asserted: every
statement in every TEST body must match one of them, or this refuses to emit.
"""
import re, sys

SRC = "/mnt/dev/maplibre-frontend/maplibre-native/test/algorithm/update_renderables.test.cpp"
text = open(SRC).read()

def nums(s):
    return [int(n) for n in re.findall(r'-?\d+', s)]

def braces(s):
    """Split a brace-list's top-level items."""
    out, depth, cur = [], 0, ''
    for ch in s:
        if ch == '{': depth += 1; cur += ch
        elif ch == '}': depth -= 1; cur += ch
        elif ch == ',' and depth == 0: out.append(cur.strip()); cur = ''
        else: cur += ch
    if cur.strip(): out.append(cur.strip())
    return out

def inner(s):
    s = s.strip()
    assert s.startswith('{') and s.endswith('}'), s
    return s[1:-1]

def data_id(s):
    """OverscaledTileID literal -> (oz, wrap, z, x, y)."""
    parts = braces(inner(s))
    if len(parts) == 3 and '{' not in s[1:-1]:
        z, x, y = nums(s)
        return (z, 0, z, x, y)
    assert len(parts) == 3, s
    oz, wrap = int(parts[0]), int(parts[1])
    z, x, y = nums(parts[2])
    return (oz, wrap, z, x, y)

def unwrapped(s):
    """UnwrappedTileID{z, x, y} with a possibly out-of-range x -> (wrap, z, x, y)."""
    z, x, y = nums(s)
    span = 1 << z
    wrap = (x - span + 1) // span if x < 0 else x // span
    return (wrap, z, x - wrap * span, y)

def rid(t):
    return "r(%d, %d, %d, %d)" % t

def did(t):
    return "d(%d, %d, %d, %d, %d)" % t

# --- statement rules -------------------------------------------------------
RULES = []
def rule(pattern):
    def wrap(fn):
        RULES.append((re.compile(pattern, re.S), fn))
        return fn
    return wrap

@rule(r'^source\.idealTiles\.emplace\(OverscaledTileID(\{.*\})\)$')
def _(m, ctx): return "source.ideal.insert(%s);" % did(data_id(m.group(1)))

@rule(r'^source\.idealTiles\.clear\(\)$')
def _(m, ctx): return "source.ideal.clear();"

@rule(r'^auto (\w+) = source\.createTileData\(OverscaledTileID(\{.*\})\)$')
def _(m, ctx):
    t = data_id(m.group(2)); ctx['vars'][m.group(1)] = t
    return "source.create_data(%s);" % did(t)

@rule(r'^(\w+)->(renderable|triedOptional|loaded) = (true|false)$')
def _(m, ctx):
    t = var_id(m.group(1), ctx)
    return "source.set(%s, |s| s.%s = %s);" % (did(t), FIELD[m.group(2)], m.group(3))

@rule(r'^source\.dataTiles\[(\{.*\})\]->(renderable|triedOptional|loaded) = (true|false)$')
def _(m, ctx):
    return "source.set(%s, |s| s.%s = %s);" % (did(data_id(m.group(1))), FIELD[m.group(2)], m.group(3))

@rule(r'^source\.createTileData\(OverscaledTileID(\{.*\})\)$')
def _(m, ctx): return "source.create_data(%s);" % did(data_id(m.group(1)))

@rule(r'^source\.dataTiles\.erase\(OverscaledTileID(\{.*\})\)$')
def _(m, ctx): return "source.erase(%s);" % did(data_id(m.group(1)))

@rule(r'^source\.dataTiles\.clear\(\)$')
def _(m, ctx): return "source.data.clear();"

@rule(r'^source\.zoomRange = \{(\d+), (\d+)\}$')
def _(m, ctx): return "source.zooms = %s..=%s;" % (m.group(1), m.group(2))

@rule(r'^source\.zoomRange\.min = (\d+)$')
def _(m, ctx): return "source.zooms = %s..=*source.zooms.end();" % m.group(1)

@rule(r'^source\.zoomRange\.max = (\d+)$')
def _(m, ctx): return "source.zooms = *source.zooms.start()..=%s;" % m.group(1)

# Bookkeeping on a raw pointer after its map entry is erased. No effect on the log, and the
# variable-to-id mapping the Rust port compares by stays valid.
# A handle taken to a tile the algorithm itself created. Binds a name to an id, emits nothing.
@rule(r'^auto \w+ = source\.dataTiles\[\{.*\}\]\.get\(\)$')
def _(m, ctx): return None

@rule(r'^\w+ = nullptr$')
def _(m, ctx): return None

@rule(r'^log\.clear\(\)$')
def _(m, ctx): return "source.log.clear();"

FIELD = {'renderable': 'renderable', 'triedOptional': 'tried_cache', 'loaded': 'loaded'}

# --- expectation entries ---------------------------------------------------
def entry(s, ctx):
    s = s.strip()
    m = re.match(r'^GetTileDataAction\{(\{.*\}), (Found|NotFound)\}$', s, re.S)
    if m:
        return "Action::Get(%s, %s)" % (did(data_id(m.group(1))), 'true' if m.group(2) == 'Found' else 'false')
    m = re.match(r'^CreateTileDataAction\{(\{.*\})\}$', s, re.S)
    if m:
        return "Action::Create(%s)" % did(data_id(m.group(1)))
    m = re.match(r'^RetainTileDataAction\{(\{.*\}), TileNecessity::(Required|Optional)\}$', s, re.S)
    if m:
        return "Action::Retain(%s, Necessity::%s)" % (did(data_id(m.group(1))), m.group(2))
    m = re.match(r'^RenderTileAction\{(\{[^{}]*\}), \*(\w+)\}$', s, re.S)
    if m:
        return "Action::Render(%s, %s)" % (rid(unwrapped(m.group(1))), did(var_id(m.group(2), ctx)))
    raise SystemExit("unhandled expectation: %r" % s)

def var_id(name, ctx):
    """The id a `tile_...` variable was declared with.

    Resolved from the declaration and never from the name. The names look like
    `tile_<oz>_<z>_<x>_<y>` but the convention is not total -- `tile_4_0_0_0` is declared as
    `{4, 0, {4, 0, 0}}` -- so deriving an id from a name would silently produce the wrong tile
    in exactly one test.
    """
    assert name in ctx['vars'], "undeclared tile variable %s" % name
    return ctx['vars'][name]


def expectation(raw, ctx):
    """Convert one EXPECT_EQ block, carrying each entry's mbgl comment across.

    The comments are the reason mbgl's expectations are readable at all -- "prefer using a child
    first", "0/0/0 has been rendered already for 1/0/0" -- and they are what lets a reader check
    this port against the original line by line rather than by counting brackets.
    """
    out, depth, cur, comment = [], 0, '', None
    body = raw[raw.index('{', raw.index('ActionLog')):]
    body = body[1:body.rindex('}')]
    for line in body.split('\n'):
        code, _, note = line.partition('//')
        cur += ' ' + code.strip()
        if note.strip():
            comment = note.strip()
        depth += code.count('{') + code.count('(') - code.count('}') - code.count(')')
        if depth == 0 and cur.strip().rstrip(','):
            text = ' '.join(cur.split()).rstrip(',')
            if text:
                rendered = "        %s," % entry(text, ctx)
                if comment:
                    rendered += "  // %s" % comment
                out.append(rendered)
            cur, comment = '', None
    return out


def statements(body):
    """Split a TEST body into statements, keeping EXPECT_EQ blocks whole."""
    out, depth, cur = [], 0, ''
    for ch in body:
        if ch in '({[':
            depth += 1
        elif ch in ')}]':
            depth -= 1
            # A block statement -- a `for` body -- ends at its closing brace and not at a
            # semicolon. Without this the block ran on into whatever followed it and the
            # combined text matched the block's own rule, silently dropping the next statement.
            if ch == '}' and depth == 0:
                out.append((cur + ch).strip()); cur = ''; continue
        if ch == ';' and depth == 0:
            out.append(cur.strip()); cur = ''
        else:
            cur += ch
    assert not cur.strip(), cur
    return out

def strip_comments(s):
    s = re.sub(r'//[^\n]*', '', s)
    return re.sub(r'/\*.*?\*/', '', s, flags=re.S)

BOILERPLATE = re.compile(
    r'^(ActionLog log|MockSource source|auto (getTileData|createTileData|retainTileData|renderTile) = \w+\()')

DECL = re.compile(r'auto (\w+) = source\.createTileData\(OverscaledTileID(\{[^;]*\})\)')
# A handle taken to a tile the algorithm itself created. Binds a name to an id and emits nothing.
HANDLE = re.compile(r'auto (\w+) = source\.dataTiles\[(\{[^;]*\})\]\.get\(\)')

def convert(name, body):
    ctx = {'vars': {}}
    raw_by_flat = {}
    clean = strip_comments(body)
    for var, lit in DECL.findall(clean) + HANDLE.findall(clean):
        ctx['vars'][var] = data_id(' '.join(lit.split()))
    lines = []
    raws = statements(body)
    cleans = statements(strip_comments(body))
    assert len(raws) == len(cleans), (name, len(raws), len(cleans))
    for raw, stmt in zip(raws, cleans):
        if not stmt.strip(): continue
        flat = ' '.join(stmt.split())
        raw_by_flat[flat] = raw
        if BOILERPLATE.match(flat): continue
        if flat.startswith('algorithm::updateRenderables('):
            args = braces(flat[len('algorithm::updateRenderables('):-1])
            extra = [a for a in args if 'maxParentOverscaleFactor' in a or re.fullmatch(r'\d+', a)]
            lines.append("source.run(%s);" % ("Some(%s)" % extra[0] if extra else "None"))
            continue
        if flat.startswith('EXPECT_EQ('):
            items = expectation(raw_by_flat[flat], ctx)
            lines.append("assert_eq!(\n    source.log,\n    [\n%s\n    ],\n);" % "\n".join(items))
            continue
        # UnwrappedTileID arrays and their loop, used only by WrappedTiles.
        m = re.match(r'^UnwrappedTileID tileIds\[\] = (\{.*\})$', flat, re.S)
        if m:
            ctx['wrapped'] = [unwrapped(p) for p in braces(inner(m.group(1)))]
            continue
        if flat.startswith('for (const auto& id : tileIds)'):
            for (w, z, x, y) in ctx['wrapped']:
                lines.append("source.ideal.insert(%s);" % did((z, w, z, x, y)))
            # One `emplace` in the source stands for every id the loop visits.
            ctx['loop_inserts'] = ctx.get('loop_inserts', 0) + len(ctx['wrapped']) - 1
            continue
        for pattern, fn in RULES:
            m = pattern.match(flat)
            if m:
                emitted = fn(m, ctx)
                if emitted is not None:
                    lines.append(emitted)
                break
        else:
            raise SystemExit("unhandled statement in %s: %r" % (name, flat))
    return lines, ctx

def audit(name, body, lines, ctx):
    """Every call in the C++ must appear in the Rust.

    The splitter once merged a `for` block with the declaration after it, dropping a
    `createTileData` -- and the result still compiled and still asserted something, which is
    precisely why a count is checked rather than trusted.
    """
    clean = strip_comments(body)
    emitted = '\n'.join(lines)
    for cpp, rust in [('source.createTileData(', 'create_data('),
                      ('source.idealTiles.emplace(', 'ideal.insert('),
                      ('EXPECT_EQ(', 'assert_eq!('),
                      ('algorithm::updateRenderables(', 'source.run(')]:
        want = clean.count(cpp)
        if cpp == 'source.idealTiles.emplace(':
            want += ctx.get('loop_inserts', 0)
        got = emitted.count(rust)
        assert want == got, "%s: %s appears %d times, %s %d" % (name, cpp, want, rust, got)


tests = re.findall(r'TEST\(UpdateRenderables, (\w+)\) \{\n(.*?)\n\}\n', text, re.S)
print("converted %d tests" % len(tests), file=sys.stderr)

def snake(n):
    return re.sub(r'(?<!^)(?=[A-Z])', '_', n).lower()

out = []
for name, body in tests:
    lines, ctx = convert(name, body)
    audit(name, body, lines, ctx)
    out.append("/// mbgl `UpdateRenderables.%s`.\n#[test]\nfn %s() {\n    let mut source = Source::new();\n%s\n}\n"
               % (name, snake(name), "\n".join("    " + l.replace("\n", "\n    ") for l in lines)))
open(sys.argv[1], 'w').write("\n".join(out))
