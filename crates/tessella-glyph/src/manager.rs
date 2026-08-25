//! The glyph manager: which ranges are held, which are still owed, and what to ask for.
//!
//! A transcription of mbgl's `GlyphManager`. A label needs the glyphs for its codepoints; those
//! arrive 256 at a time in range files, and a style may name several font stacks. This decides
//! what to fetch and remembers what came back.
//!
//! # Absence is remembered per range, not per glyph
//!
//! A font does not contain every codepoint in a range it serves. Asking "is this glyph missing
//! because we have not fetched it, or because the font does not have it" is the question that
//! decides whether to spend a request, and it cannot be answered from the glyph table alone —
//! an absent glyph looks identical either way.
//!
//! So what is remembered is which *ranges* have been parsed. A codepoint whose range is parsed
//! and which is not in the table is definitively absent, and is never asked for again. Without
//! that, every label containing one unusual character would re-request its whole range on every
//! tile, forever, and the request would succeed every time.
//!
//! # An empty response settles a range; a failed one does not
//!
//! mbgl draws that line and it is the right one. A `204` or an empty body means the origin has
//! nothing for that range: the range is settled, glyphless, and asking again would waste a
//! round trip on every tile. A transport error means the answer is unknown, so the range stays
//! owed and the next tile that needs it tries again.

use std::collections::{BTreeMap, BTreeSet};

use tessella_storage::source::{FetchError, FileSource};
use tessella_storage::url::percent_encode;

use crate::pbf::{self, Glyph, GlyphError, Range};

/// The fonts a label asks for, in order of preference.
///
/// mbgl's `FontStack`. The order is significant: it is what the origin serves under, and two
/// stacks with the same fonts in a different order are different stacks with different URLs.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct FontStack(pub Vec<String>);

impl FontStack {
    /// A stack from anything string-like.
    pub fn new<I, S>(fonts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self(fonts.into_iter().map(Into::into).collect())
    }

    /// The name the origin serves this stack under: the fonts joined by commas.
    #[must_use]
    pub fn name(&self) -> String {
        self.0.join(",")
    }
}

/// Why glyphs could not be loaded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LoadError {
    /// The range could not be fetched.
    #[error("fetching glyphs for `{stack}` {range}: {source}")]
    Fetch {
        /// The font stack asked for.
        stack: String,
        /// The range asked for.
        range: String,
        /// What went wrong.
        #[source]
        source: FetchError,
    },
    /// The origin answered with a status that is not a glyph range.
    #[error("glyphs for `{stack}` {range} returned {status}")]
    Status {
        /// The font stack asked for.
        stack: String,
        /// The range asked for.
        range: String,
        /// What came back.
        status: u16,
    },
    /// The range did not parse.
    #[error("parsing glyphs for `{stack}` {range}: {source}")]
    Malformed {
        /// The font stack asked for.
        stack: String,
        /// The range asked for.
        range: String,
        /// What went wrong.
        #[source]
        source: GlyphError,
    },
}

/// One font stack's glyphs, and the ranges already settled for it.
#[derive(Debug, Default)]
struct Entry {
    glyphs: BTreeMap<u32, Glyph>,
    /// Ranges that have been answered — including answered as empty. A codepoint in one of
    /// these that is not in `glyphs` is absent from the font rather than unfetched.
    settled: BTreeSet<Range>,
}

/// Holds glyph ranges for every font stack a style names.
///
/// Process-scoped (§5.1, §5.5): the atlas and the glyphs behind it are shared, and a second
/// view over the same style costs no fetch. Nothing here is per view.
#[derive(Debug)]
pub struct GlyphManager {
    url: String,
    entries: BTreeMap<FontStack, Entry>,
    requests: u64,
}

impl GlyphManager {
    /// A manager serving from a style's `glyphs` URL template.
    #[must_use]
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            entries: BTreeMap::new(),
            requests: 0,
        }
    }

    /// The URL a range of a stack is fetched from.
    ///
    /// `{fontstack}` and `{range}` are the only tokens, and an unrecognised one survives verbatim
    /// — mbgl's `replaceTokens` puts it back braces and all, because a URL may legitimately
    /// contain braces and dropping them yields a 404 with no clue why.
    #[must_use]
    pub fn url_for(&self, stack: &FontStack, range: Range) -> String {
        let mut out = String::with_capacity(self.url.len() + 32);
        let mut rest = self.url.as_str();
        while let Some(open) = rest.find('{') {
            out.push_str(&rest[..open]);
            rest = &rest[open..];
            let Some(close) = rest.find('}') else {
                break;
            };
            match &rest[1..close] {
                "fontstack" => out.push_str(&percent_encode(&stack.name())),
                "range" => out.push_str(&range.to_string()),
                _ => out.push_str(&rest[..=close]),
            }
            rest = &rest[close + 1..];
        }
        out.push_str(rest);
        out
    }

    /// Ranges this stack still owes for these codepoints, in order.
    ///
    /// Public because it is what a scheduler wants: the whole point of §5.4's fan-out is to
    /// issue these together rather than one at a time, and a caller that can only ask for them
    /// one by one cannot.
    #[must_use]
    pub fn owed(&self, stack: &FontStack, codepoints: impl IntoIterator<Item = u32>) -> Vec<Range> {
        let settled = self.entries.get(stack);
        let mut ranges = BTreeSet::new();
        for codepoint in codepoints {
            let Some(range) = Range::of(codepoint) else {
                // Above the BMP: served by the local rasterizer, not by a range file.
                continue;
            };
            if settled.is_none_or(|entry| !entry.settled.contains(&range)) {
                ranges.insert(range);
            }
        }
        ranges.into_iter().collect()
    }

    /// Fetches and records one range.
    ///
    /// # Errors
    ///
    /// [`LoadError`] when the range could not be fetched or did not parse. The range is left
    /// unsettled in that case, so a later call tries again — the answer is unknown rather than
    /// known to be empty.
    pub fn load_range(
        &mut self,
        stack: &FontStack,
        range: Range,
        files: &dyn FileSource,
    ) -> Result<(), LoadError> {
        if self
            .entries
            .get(stack)
            .is_some_and(|entry| entry.settled.contains(&range))
        {
            return Ok(());
        }

        let url = self.url_for(stack, range);
        self.requests += 1;
        let response = files.fetch(&url).map_err(|source| LoadError::Fetch {
            stack: stack.name(),
            range: range.to_string(),
            source,
        })?;

        // An origin with nothing for this range settles it glyphless: the font genuinely does
        // not serve those codepoints, and asking again on the next tile costs a round trip to
        // be told so again.
        let glyphs = if response.is_absent() || response.body.is_empty() {
            Vec::new()
        } else if !response.is_ok() {
            return Err(LoadError::Status {
                stack: stack.name(),
                range: range.to_string(),
                status: response.status,
            });
        } else {
            pbf::parse(range, &response.body).map_err(|source| LoadError::Malformed {
                stack: stack.name(),
                range: range.to_string(),
                source,
            })?
        };

        let entry = self.entries.entry(stack.clone()).or_default();
        for glyph in glyphs {
            entry.glyphs.insert(glyph.id, glyph);
        }
        entry.settled.insert(range);
        Ok(())
    }

    /// Fetches whatever these codepoints need that is not already held.
    ///
    /// # Errors
    ///
    /// The first [`LoadError`] encountered. Ranges loaded before it stay loaded.
    pub fn load(
        &mut self,
        stack: &FontStack,
        codepoints: impl IntoIterator<Item = u32>,
        files: &dyn FileSource,
    ) -> Result<(), LoadError> {
        for range in self.owed(stack, codepoints) {
            self.load_range(stack, range, files)?;
        }
        Ok(())
    }

    /// One glyph, if it is held.
    #[must_use]
    pub fn glyph(&self, stack: &FontStack, id: u32) -> Option<&Glyph> {
        self.entries.get(stack)?.glyphs.get(&id)
    }

    /// Whether this codepoint's fate is known: either held, or its range settled without it.
    ///
    /// The distinction [`Self::glyph`] cannot make on its own, and the one a shaper needs
    /// before it decides a character is undrawable rather than merely late.
    #[must_use]
    pub fn is_resolved(&self, stack: &FontStack, id: u32) -> bool {
        let Some(entry) = self.entries.get(stack) else {
            return false;
        };
        Range::of(id).is_some_and(|range| entry.settled.contains(&range))
    }

    /// How many ranges have been asked for over this manager's life.
    ///
    /// §9.3's flatness counter for glyphs: N views over one style must not multiply it.
    #[must_use]
    pub const fn requests(&self) -> u64 {
        self.requests
    }

    /// How many glyphs are held, across every stack.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.values().map(|entry| entry.glyphs.len()).sum()
    }

    /// Whether anything is held.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Drops every stack but these.
    ///
    /// mbgl's `evict`, called when a style changes. A stack no layer names any more is holding
    /// an atlas' worth of bitmaps for nothing.
    pub fn evict(&mut self, keep: &BTreeSet<FontStack>) {
        self.entries.retain(|stack, _| keep.contains(stack));
    }
}
