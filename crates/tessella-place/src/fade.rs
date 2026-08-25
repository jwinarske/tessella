//! Symbol opacity, and the fades that carry a label on and off the map.
//!
//! A transcription of mbgl's `OpacityState` and `JointOpacityState`. Placement decides each
//! frame whether a symbol is placed; this turns that boolean into the opacity it draws at, so a
//! label that loses a collision fades out rather than vanishing between two frames.
//!
//! # Why this is what §6.5 is about
//!
//! The still-frame guarantee says a settled map sends nothing. A fade is the one thing that
//! keeps changing while nothing else does — the camera has stopped, the tiles have arrived, and
//! a label is still on its way to opaque. So a fade counts as churn *until it settles*, and then
//! must go completely quiet. [`Fades::settled`] is that question, and a fade that never quite
//! reached 1.0 would keep a map awake forever.
//!
//! # The direction of a fade comes from the previous frame, not this one
//!
//! mbgl steps opacity by `prev.placed ? +increment : -increment` and *then* stores the new
//! `placed`. So on the frame a symbol loses its collision, its opacity still rises one step; it
//! begins falling the frame after. That one-frame lag is transcribed rather than corrected: it
//! is what stops a label flickering when a collision result oscillates between two frames, and
//! smoothing it out here would trade a rare stale frame for a common flicker.

use std::collections::BTreeMap;

/// mbgl's default transition duration, in seconds.
pub const DEFAULT_FADE_SECONDS: f32 = 0.3;

/// One channel's opacity — a symbol's text, or its icon.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Opacity {
    /// What it draws at, in 0..1.
    pub opacity: f32,
    /// Whether placement wants it visible.
    pub placed: bool,
}

impl Opacity {
    /// A symbol appearing for the first time.
    ///
    /// `skip_fade` is for symbols that entered from just outside the viewport: they were never
    /// visible, so there is nothing to fade *from*, and fading them in would show the user a
    /// label materialising where one had simply scrolled into view.
    #[must_use]
    pub fn new(placed: bool, skip_fade: bool) -> Self {
        Self {
            opacity: if skip_fade && placed { 1.0 } else { 0.0 },
            placed,
        }
    }

    /// One frame's step.
    ///
    /// The direction is the *previous* frame's placement; see the module note.
    #[must_use]
    pub fn step(self, increment: f32, placed: bool) -> Self {
        let delta = if self.placed { increment } else { -increment };
        Self {
            opacity: (self.opacity + delta).clamp(0.0, 1.0),
            placed,
        }
    }

    /// Whether it draws nothing and is not on its way back.
    #[must_use]
    pub fn is_hidden(self) -> bool {
        self.opacity == 0.0 && !self.placed
    }

    /// Whether it has finished moving.
    ///
    /// Opaque and placed, or transparent and not. Anything else is mid-fade, and §6.5 counts it
    /// as churn.
    #[must_use]
    pub fn is_settled(self) -> bool {
        if self.placed {
            self.opacity >= 1.0
        } else {
            self.opacity <= 0.0
        }
    }
}

/// A symbol's two channels, which fade together but not necessarily in step.
///
/// A label whose icon collides and whose text does not is a real arrangement — `icon-optional`
/// and `text-optional` exist for it — so the two carry their own opacity.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Joint {
    /// The icon's opacity.
    pub icon: Opacity,
    /// The text's opacity.
    pub text: Opacity,
}

impl Joint {
    /// A symbol appearing for the first time.
    #[must_use]
    pub fn new(placed_text: bool, placed_icon: bool, skip_fade: bool) -> Self {
        Self {
            icon: Opacity::new(placed_icon, skip_fade),
            text: Opacity::new(placed_text, skip_fade),
        }
    }

    /// One frame's step for both channels.
    #[must_use]
    pub fn step(self, increment: f32, placed_text: bool, placed_icon: bool) -> Self {
        Self {
            icon: self.icon.step(increment, placed_icon),
            text: self.text.step(increment, placed_text),
        }
    }

    /// Whether neither channel draws anything.
    #[must_use]
    pub fn is_hidden(self) -> bool {
        self.icon.is_hidden() && self.text.is_hidden()
    }

    /// Whether both channels have finished moving.
    #[must_use]
    pub fn is_settled(self) -> bool {
        self.icon.is_settled() && self.text.is_settled()
    }
}

/// Every symbol's fade state, carried between frames.
///
/// Keyed by the cross-tile id, which is what makes a label keep its opacity when the tile under
/// it is replaced at a zoom crossing — the same label in a new tile is the same label, and
/// re-fading it is the "symbol pop" §13.3 asks for zero of.
#[derive(Debug, Default)]
pub struct Fades {
    states: BTreeMap<u32, Joint>,
}

impl Fades {
    /// No symbols yet.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Advances every symbol named in `placements`, and drops those that have faded away.
    ///
    /// `increment` is how far a fade moves this frame: the frame's duration over the fade's.
    /// Symbols not in `placements` are gone from the map entirely — their tile was released —
    /// and are dropped rather than faded, since there is nothing left to draw them from.
    pub fn step<I>(&mut self, increment: f32, placements: I, skip_fade: bool)
    where
        I: IntoIterator<Item = (u32, bool, bool)>,
    {
        let mut next: BTreeMap<u32, Joint> = BTreeMap::new();
        for (id, placed_text, placed_icon) in placements {
            let state = match self.states.get(&id) {
                Some(previous) => previous.step(increment, placed_text, placed_icon),
                None => Joint::new(placed_text, placed_icon, skip_fade),
            };
            // A symbol that has finished fading out draws nothing and will not come back on its
            // own; keeping it would grow this map for the life of the process.
            if !state.is_hidden() {
                next.insert(id, state);
            }
        }
        self.states = next;
    }

    /// A symbol's current opacity, if it has one.
    #[must_use]
    pub fn get(&self, id: u32) -> Option<Joint> {
        self.states.get(&id).copied()
    }

    /// Whether every fade has finished — §6.5's question.
    ///
    /// True for an empty set: a map with no symbols is as settled as a map whose symbols have
    /// all arrived.
    #[must_use]
    pub fn settled(&self) -> bool {
        self.states.values().all(|state| state.is_settled())
    }

    /// How many symbols are still moving.
    ///
    /// The §9.3 counter: it must reach zero and stay there while the camera is still.
    #[must_use]
    pub fn fading(&self) -> usize {
        self.states
            .values()
            .filter(|state| !state.is_settled())
            .count()
    }

    /// How many symbols have state.
    #[must_use]
    pub fn len(&self) -> usize {
        self.states.len()
    }

    /// Whether nothing is tracked.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.states.is_empty()
    }
}

/// How far a fade moves in a frame of `elapsed` seconds.
///
/// A duration of zero means transitions are off, and everything snaps: mbgl returns 1 there
/// rather than dividing, and a caller that divided would produce an infinity that propagates
/// into every opacity.
#[must_use]
pub fn increment(elapsed_seconds: f32, fade_seconds: f32) -> f32 {
    if fade_seconds <= 0.0 {
        return 1.0;
    }
    elapsed_seconds / fade_seconds
}
