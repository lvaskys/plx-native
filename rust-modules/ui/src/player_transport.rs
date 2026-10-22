//! Shared player chrome, extracted from appkit::player_hud. Callers resolve
//! application state into these values; this library owns no session or provider.
use crate::{
    consts::{SCR_H, SCR_W},
    theme,
    widgets::Spinner,
    Env, Painter, Rect, View,
};
use std::ffi::CString;

fn hud_env() -> Env {
    Env::inert()
}
// Existing media-chrome type-scale exception, shared with the Plex player.
pub const HUD_TITLE_SZ: i32 = 54;
pub const SCRIM_H: f32 = 470.0;
pub const SB_X: f32 = crate::consts::MARGIN_X;
pub const fn sb_w() -> f32 {
    SCR_W - 2.0 * SB_X
}
pub const SB_Y: f32 = SCR_H - 198.0;
pub const SB_H: f32 = 8.0;
/// the shared playback clock ([`crate::fmt::clock`]) with the HUD's leading '-' for remaining
pub fn fmt_time(ns: i64, neg: bool) -> String {
    let c = crate::fmt::clock(ns / 1_000_000);
    if neg {
        format!("-{c}")
    } else {
        c
    }
}

/// How long [`TransportMark::Play`] stands after a resume — "a couple of seconds", per the owner:
/// the mark answers *did that press land*, and a play glyph held for the whole film would be
/// saying "playing" to someone who is watching a moving picture.
pub const PLAY_MARK_MS: u32 = 2_000;

/// What the state read-out shows this frame. See the caller’s state resolver for the rule.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TransportMark {
    /// playing steadily — the slot is EMPTY, which is the common case and the point of the whole
    /// design: a mark that is always up says nothing
    None,
    /// paused
    Pause,
    /// the playhead is travelling BACKWARDS (scrub, hop or seek burst)
    Rewind,
    /// the playhead is travelling FORWARDS
    FastForward,
    /// resumed within the last [`PLAY_MARK_MS`]
    Play,
    /// the inline [`Spinner`] — the pipeline is being waited on with NO travel to show
    Working,
}

/// draw a clock string centred on `cx` as ONE label box, clamped so it stays inside [lo, hi].
/// (The old last-':'-anchor read visibly lopsided once the clock grew to H:MM:SS — "1:05" left of
/// the knob vs "53" right.) The box width is measured on a same-shape template with every digit
/// as '0', so the box — and the returned extents — stay stable while digits tick instead of
/// wobbling with proportional digit widths. Returns the label's (left, right) x extents.
fn draw_clock(
    p: Painter,
    text: &str,
    cx: f32,
    y: f32,
    sz: i32,
    col: [f32; 4],
    lo: f32,
    hi: f32,
    measure: &dyn plx_machine::machine::Measure,
) -> (f32, f32) {
    let template: String = text
        .chars()
        .map(|c| if c.is_ascii_digit() { '0' } else { c })
        .collect();
    let w = measure.width_str(&template, sz, true);
    let half = w * 0.5;
    let cx = cx.clamp(lo + half, (hi - half).max(lo + half));
    if let Ok(cs) = CString::new(text) {
        p.text(cs.as_ptr(), cx, y, sz, col, 1, 1);
    }
    (cx - half, cx + half)
}

/// The transport's dark ground: transparent at its top edge, `theme::scrim_black(0.86)` at the
/// panel bottom, [`SCRIM_H`] tall. Drawn under every transport, the player's and the trailer's.
pub fn draw_scrim(p: Painter) {
    p.rect(
        Rect::new(0.0, SCR_H - SCRIM_H, SCR_W, SCRIM_H),
        0.0,
        theme::scrim_black(0.0),
        theme::scrim_black(0.86),
        0.0,
    );
}

/// The line over a transport's display title. Non-owning `*const c_char`, the `Label` rule
/// (`ui/CLAUDE.md`): keep the `CString` (or the session's fixed buffer) alive across the draw.
#[derive(Clone, Copy, Debug)]
pub enum Kicker {
    /// an episode's own address and name ("S1, E1 · Pilot"): primary ink, bold — it IS the
    /// episode's title, with the show's name as the display title under it
    Episode(*const std::os::raw::c_char),
    /// a context label (`metadata::context_label`: an extra's kind, a movie's route ctxline):
    /// secondary ink, regular — it only says what the display title is
    Context(*const std::os::raw::c_char),
}

/// The title block above the playbar: the [`Kicker`] line over the [`HUD_TITLE_SZ`] display
/// title. (Apple-TV layout.)
pub fn draw_title(p: Painter, kicker: Kicker, title: *const std::os::raw::c_char) {
    let (text, ink, bold) = match kicker {
        Kicker::Episode(text) => (text, theme::TEXT_PRIMARY, 1),
        Kicker::Context(text) => (text, theme::TEXT_SECONDARY, 0),
    };
    p.text(
        text,
        SB_X,
        SCR_H - 312.0,
        theme::size::CAPTION,
        ink,
        0,
        bold,
    );
    p.text(
        title,
        SB_X,
        SCR_H - 278.0,
        HUD_TITLE_SZ,
        theme::TEXT_PRIMARY,
        0,
        1,
    );
}

/// How the playhead is drawn — see [`draw_playbar`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Knob {
    /// the scrubber holds the focus ring: a glowing knob
    Focused,
    /// a scrub preview is being dragged with the ring elsewhere: a plain knob
    Held,
    /// neither: a thin tick
    Tick,
}

/// Everything [`draw_playbar`] draws from, resolved by its caller. A value rather than a read of
/// the transport's globals inside the draw, because several surfaces draw this bar and resolve it
/// differently: the player route from its own scrub gesture, seek target and focus ring
/// (the caller’s live-state resolver), and the detail page's full-trailer transport from a preview session that
/// has none of the three.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Playbar {
    /// the DISPLAYED playhead (a seek target, a scrub preview or the published position)
    pub pos_ns: i64,
    pub dur_ns: i64,
    pub knob: Knob,
    /// the state read-out beside the elapsed clock, from the caller’s state resolver
    pub mark: TransportMark,
    /// drives the inline spinner's rotation
    pub now: u32,
}

/// The playbar: scrubber, playhead knob, elapsed/remaining clocks and the state read-out. The
/// player HUD and the detail page's full-trailer transport both draw it, so the two cannot drift.
pub fn draw_playbar(p: Painter, bar: Playbar, measure: &dyn plx_machine::machine::Measure) {
    let e = hud_env();
    let white = theme::TEXT_PRIMARY;
    let dim = theme::TEXT_SECONDARY;
    let track = theme::RAIL_TRACK;
    let sx = SB_X;
    let sw = sb_w();
    let sy = SB_Y;
    let sh = SB_H;
    let dispos = bar.pos_ns;
    let dur = bar.dur_ns;
    let now = bar.now;
    let frac = if dur > 0 {
        (dispos as f64 / dur as f64).clamp(0.0, 1.0)
    } else {
        0.0
    };
    p.rect(Rect::new(sx, sy, sw, sh), sh * 0.5, track, track, 0.0);
    let fw = (sw as f64 * frac) as f32;
    if fw > sh * 0.5 {
        p.rrect(Rect::new(sx, sy, fw, sh), sh * 0.5, 0.0, white);
    } else if fw > 0.0 {
        p.rrect(Rect::new(sx, sy, fw, sh), fw * 0.5, 0.0, white);
    }
    // playhead: a focus-glowing knob when the scrubber is focused, a plain knob while scrubbing,
    // else a thin tick.
    let hx = sx + fw;
    let cy = sy + sh * 0.5;
    if bar.knob == Knob::Focused {
        let glow = theme::white(0.22);
        p.rect(
            Rect::new(hx - 17.0, cy - 17.0, 34.0, 34.0),
            17.0,
            glow,
            glow,
            0.0,
        );
        p.rect(
            Rect::new(hx - 11.0, cy - 11.0, 22.0, 22.0),
            11.0,
            white,
            white,
            0.0,
        );
    } else if bar.knob == Knob::Held {
        p.rect(
            Rect::new(hx - 9.0, cy - 9.0, 18.0, 18.0),
            9.0,
            white,
            white,
            0.0,
        );
    } else {
        p.rect(
            Rect::new(hx - 1.5, sy - 4.0, 3.0, sh + 8.0),
            0.0,
            white,
            white,
            0.0,
        );
    }

    // elapsed under the playhead (':' centered on the knob, clamped to the bar); remaining at the
    // right — hidden once the moving elapsed label would overlap it (near the end of the movie).
    let ty = sy + 30.0;
    let (el_l, el_r) = draw_clock(
        p,
        &fmt_time(dispos, false),
        hx,
        ty,
        theme::size::CAPTION,
        white,
        sx,
        sx + sw,
        measure,
    );
    let rem = fmt_time(dur - dispos, true);
    // MEASURE the label, never estimate it. `chars * CAPTION * 0.52` was an Arial-calibrated
    // constant guarding a real behaviour — the remaining clock hides before the moving elapsed
    // clock can reach it — and it only ever worked because it over-estimated: under Arial it
    // returned ~99.8px against a true ~85px. Under the shipped Inter that margin had already
    // halved, and freezing tabular figures (which the clock's own template idiom requires, see
    // `draw_clock`) widens numerals further, leaving about a pixel of slack. At that point the
    // guard stops guarding and the two clocks can overlap near the end of a long item.
    // Same '0'-template `draw_clock` uses, so the two agree by construction: with tabular figures
    // the template's width IS the real string's width at every tick.
    let rem_tmpl: String = rem
        .chars()
        .map(|c| if c.is_ascii_digit() { '0' } else { c })
        .collect();
    let rem_w = measure.width_str(&rem_tmpl, theme::size::CAPTION, true);
    let rem_l = sx + sw - rem_w;
    let rem_shown = el_r + 20.0 < rem_l;
    if rem_shown {
        if let Ok(cs) = CString::new(rem.as_str()) {
            p.text(cs.as_ptr(), sx + sw, ty, theme::size::CAPTION, dim, 2, 0);
        }
    }
    // transport state indicator just past the elapsed clock — the four-state READ-OUT resolved by
    // the caller’s state resolver (rewind / pause / fast-forward / play, plus the narrowed spinner), and
    // NOTHING while playing steadily. A read-out, not an action toggle: nothing here is focusable
    // or hit-tested, and the control row stays the three discs it has always been.
    // Gated on `busy`, not on `loading()`: with `loading()` the transport lit the same spinner the
    // centred read-out was already showing, for the whole of every load AND every seek. Centered on
    // the clock's line box; drops to the clock's LEFT when the right side is against the remaining
    // label / screen edge.
    let mark = bar.mark;
    if mark != TransportMark::None {
        // pause bars under-fill their viewBox (14/24 tall) — a 30px box renders ~17px of ink,
        // matching the CAPTION clock's cap height so the glyph reads as the label's size. The two
        // travel marks are drawn to that SAME 14-unit band (see `Icon::Rewind`), which is what lets
        // one box serve the whole family without the slot changing weight as the state flips.
        let isz = 30.0f32;
        // The odd member of the family — `play.svg` is authored to 16 units where the other three
        // are 14 — is corrected by asking `icons::band` rather than by a constant here. See that
        // function: the metric is a property of the asset, and this was the second screen to
        // transcribe one out of an SVG by hand.
        // **The gap is measured to the INK, not to the box.** Every member of this family carries a
        // different left bearing inside its 24-unit viewBox — pause's bars open at x=7, play's
        // triangle at x=6, the travel marks at x=2.6 — so ONE box origin gives each state a
        // visibly different gap after the clock, and the slot appears to twitch as the state flips.
        // Measuring to the ink makes the gap the eye sees a single number. It is also what the old
        // spacing really was: a box placed 14px out put pause's ink at 14 + 7/24*30 ~= 23px, which
        // is the gap that read as too wide. The bearings come from `icons::ink_x`, not from a table
        // here — they are the asset's, and this screen was the second place to copy them out.
        const GAP: f32 = 14.0;
        let glyph = match mark {
            TransportMark::Pause => Some(crate::icons::Icon::Pause),
            TransportMark::Play => Some(crate::icons::Icon::Play),
            TransportMark::Rewind => Some(crate::icons::Icon::Rewind),
            TransportMark::FastForward => Some(crate::icons::Icon::FastForward),
            TransportMark::Working | TransportMark::None => None,
        };
        let ink = glyph.map_or((0.0, 1.0), crate::icons::ink_x);
        let icy = ty + plx_gfx::text::text_height(theme::size::CAPTION, 1) * 0.5; // vertical center of the clock line
                                                                                  // scaled so every member of the family lands the SAME height of ink in this one box
        let bs = glyph.map_or(isz, |g| {
            isz * crate::icons::band(crate::icons::Icon::Pause) / crate::icons::band(g)
        });
        // **Rewind sits to the LEFT of the clock; everything else to the right.** The mark points
        // the way the playhead is travelling, so `<<` after the time would point back at the number
        // it is leaving. The right-hand placement still falls back to the left when the remaining
        // label or the screen edge crowds it, which is the case this branch was originally for.
        let need = (ink.1 - ink.0) * bs + 6.0;
        let room_right = el_r + GAP + need < if rem_shown { rem_l - 8.0 } else { sx + sw };
        let on_left = mark == TransportMark::Rewind || !room_right;
        // Placed by ink on whichever side it lands: the trailing edge sits GAP before the clock's
        // left, or the leading edge GAP after the clock's right.
        let bx = if on_left {
            el_l - GAP - ink.1 * bs
        } else {
            el_r + GAP - ink.0 * bs
        };
        match glyph {
            Some(id) => crate::icons::draw(p, id, Rect::new(bx, icy - bs * 0.5, bs, bs), white),
            // placeholder-exempt: the transport's inline busy mark on a button (R_INLINE), reported by the playback pipeline, not a content stand-in
            None => Spinner::new(bx + isz * 0.5, icy, Spinner::R_INLINE)
                .phase(now)
                .tint(white)
                .draw(&e, p),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shared_clock_stays_inside_rail_and_stable_as_digits_change() {
        let _guard = plx_base::testlock::serial();
        let p = Painter::recording();
        let measure = plx_machine::machine::BareMeasure;
        for cx in [SB_X, SB_X + sb_w() * 0.5, SB_X + sb_w()] {
            let first = draw_clock(
                p,
                "1:11:11",
                cx,
                SB_Y,
                theme::size::CAPTION,
                theme::TEXT_PRIMARY,
                SB_X,
                SB_X + sb_w(),
                &measure,
            );
            let next = draw_clock(
                p,
                "1:59:59",
                cx,
                SB_Y,
                theme::size::CAPTION,
                theme::TEXT_PRIMARY,
                SB_X,
                SB_X + sb_w(),
                &measure,
            );
            assert_eq!(first, next);
            assert!(first.0 >= SB_X && first.1 <= SB_X + sb_w());
        }
        draw_scrim(p);
        draw_title(
            p,
            Kicker::Episode(c"Episode 23".as_ptr()),
            c"An embedded player".as_ptr(),
        );
        draw_playbar(
            p,
            Playbar {
                pos_ns: 83_400_000_000,
                dur_ns: 120_000_000_000,
                knob: Knob::Tick,
                mark: TransportMark::None,
                now: 0,
            },
            &measure,
        );
        assert_eq!(fmt_time(83_400_000_000, false), "1:23");
        assert_eq!(fmt_time(36_600_000_000, true), "-0:36");
    }
}
