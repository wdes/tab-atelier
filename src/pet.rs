// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Desktop "screen mate" pets — eSheep-compatible (the Mini Owl, rainbow sheep,
//! Blue Ham Ham).
//!
//! Each pet is a pair under `/usr/share/tab-atelier/pets/` (dev: `./assets/pets/`):
//! a baked sprite sheet `<id>.png` (a `tilesx`×`tilesy` grid, alpha-keyed and
//! palette-quantized offline by `tools/bake-pets`) and its animation script
//! `<id>.xml` (the desktopPet format). This module parses the XML into an
//! animation model and runs the state machine — the pet walks the floor, climbs
//! the walls, crosses the ceiling, and falls under gravity, driven by each
//! animation's per-step motion and its edge/gravity transitions. Extra [`Surface`]
//! ledges (the tab bar, …) let it hop between levels. The gpui overlay in
//! `app.rs` reads [`Pet::current_tile`] / [`Pet::pos`] each frame, clips the sheet
//! to the active tile, and can [`Pet::grab`]/[`Pet::release`] it for drag-and-drop.
//!
//! [`PetOverlay`] runs a whole herd: each summon click raises the herd's target
//! size and adds a pet, grass sprouts on the tab-line ledges, sheep stop to graze
//! it down ([`Pet::start_grazing`]), and grazers wander in on their own — but
//! never past the asked-for count, so the herd can't run away.
//!
//! Dead-end animations (empty `<next>`) are desktopPet's death/kill/effect
//! sequences (`alien_kill`, `blank_die`, …) that end the pet's life via a
//! spawn/child system we don't model. Upstream respawns the pet there
//! (`FormPet.SetNewAnimation`: `if (id < 0) … spawn!`), and so do we — a
//! [`Pet::respawn`] to the start point — so a pet can never freeze (e.g. stuck
//! forever at the top of the screen). Unknown animation ids fall back the same.

#![cfg(feature = "pets")]

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use gpui::{Context, InteractiveElement as _, IntoElement, ParentElement as _, Styled as _, canvas, div, img, px};

/// Which screen edge a border transition applies to. eSheep tags each border
/// `<next>` with `only=`: `vertical` = the side walls, `horizontal+` = the top,
/// `horizontal-` = the floor, and `none`/absent = any edge. Transitions scoped to
/// other windows or the taskbar (`only="window"/"taskbar"`) are dropped — we're a
/// single fullscreen surface, so those edges don't exist.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Edge {
    Any,
    Side,
    Top,
    Bottom,
}

/// One eSheep animation: a frame sequence with per-step movement + transitions.
#[derive(Clone, Debug)]
struct Anim {
    frames: Vec<u32>,
    interval_ms: f32,
    /// Per-step movement at the sequence start/end (sprite-space). The engine
    /// lerps between them across the frames. `x` is mirrored by `facing`; `y` is
    /// not (gravity is always down). Some exotic animations use expressions like
    /// `-imageW*0.45` for these — unparseable, so they read as 0.
    start: (f32, f32),
    end: (f32, f32),
    /// Flip the pet's facing when this sequence ends (the `flip` action).
    flip: bool,
    /// `(probability, target_id)` transitions evaluated at sequence end.
    next: Vec<(u32, u32)>,
    /// `(edge, probability, target_id)` transitions when a screen edge is hit.
    border_next: Vec<(Edge, u32, u32)>,
    /// Where to go when airborne (the `<gravity><next>`). Present on ground
    /// animations (walk/run/idle) ⇒ walking off an edge or being dropped falls.
    gravity_next: Option<u32>,
}

/// The parsed pet definition: sheet geometry + animations.
pub struct PetDef {
    pub tilesx: u32,
    pub tilesy: u32,
    /// `Rc<Anim>` so the 20 fps tick's working copy is a refcount bump —
    /// the `Anim` deep clone (3 Vecs) ran per pet per step while visible.
    anims: HashMap<u32, std::rc::Rc<Anim>>,
    /// Animation to start in (the `walk` id, or the lowest id as a fallback).
    start: u32,
    /// The `fall` named animation, if any — desktopPet's `Fall` key, played when
    /// the pet is released from a drag so it drops instead of resuming whatever
    /// (e.g. wall-climbing) animation it was grabbed in.
    fall: Option<u32>,
    /// Astonished/held animation played while grabbed — desktopPet's `drag` key,
    /// or `scream` as a stand-in. `None` ⇒ the pet just freezes its current frame.
    grabbed: Option<u32>,
    /// The `eat` animation, if the pet grazes (the sheep). `None` ⇒ it ignores
    /// grass. Played in place while nibbling a grass tuft.
    eat: Option<u32>,
    /// Whether any animation has a `<gravity>` fall. Pets without one get a
    /// synthetic downward drift when airborne so a dropped pet doesn't hover;
    /// pets with one are left to their own aerial animations.
    has_gravity: bool,
    /// A flyer (the owl) ignores gravity entirely — when airborne it flies
    /// (drifts around the air) instead of falling. Set by [`load_pet`].
    flyer: bool,
}

impl PetDef {
    /// Parse a desktopPet animation XML. Returns `None` if it has no usable
    /// `<image>` geometry or no animations.
    #[must_use]
    pub fn parse(xml: &str) -> Option<Self> {
        let doc = roxmltree::Document::parse(xml).ok()?;
        let root = doc.root_element();
        let child = |parent: roxmltree::Node<'_, '_>, tag: &str| -> Option<String> {
            parent
                .children()
                .find(|n| n.has_tag_name(tag))
                .and_then(|n| n.text())
                .map(|s| s.trim().to_string())
        };
        let num = |parent: roxmltree::Node<'_, '_>, tag: &str| -> Option<f32> {
            child(parent, tag).and_then(|s| s.parse().ok())
        };

        let image = root.children().find(|n| n.has_tag_name("image"))?;
        let tilesx = num(image, "tilesx")? as u32;
        let tilesy = num(image, "tilesy")? as u32;
        if tilesx == 0 || tilesy == 0 {
            return None;
        }

        let anims_node = root.children().find(|n| n.has_tag_name("animations"))?;
        let mut anims: HashMap<u32, std::rc::Rc<Anim>> = HashMap::new();
        let mut start = u32::MAX;
        let mut walk_id = None;
        let mut fall_id = None;
        let (mut drag_id, mut scream_id, mut eat_id) = (None, None, None);
        for a in anims_node.children().filter(|n| n.has_tag_name("animation")) {
            let Some(id) = a.attribute("id").and_then(|s| s.parse::<u32>().ok()) else {
                continue;
            };
            match child(a, "name").as_deref() {
                Some("walk") => walk_id = Some(id),
                Some("fall") => fall_id = Some(id),
                Some("drag") => drag_id = Some(id),
                Some("scream") => scream_id = Some(id),
                Some("eat") => eat_id = Some(id),
                _ => {}
            }
            let seq = a.children().find(|n| n.has_tag_name("sequence"));
            let frames: Vec<u32> = seq.map_or_else(Vec::new, |s| {
                s.children()
                    .filter(|n| n.has_tag_name("frame"))
                    .filter_map(|n| n.text().and_then(|t| t.trim().parse().ok()))
                    .collect()
            });
            if frames.is_empty() {
                continue;
            }
            // Per-step movement at the sequence's start/end (the engine lerps
            // between them). eSheep `x` is negative = leftward; `y` positive =
            // down. Expression forms (`-imageW*0.45`, …) fail to parse ⇒ 0.
            let start_n = a.children().find(|n| n.has_tag_name("start"));
            let end_n = a.children().find(|n| n.has_tag_name("end"));
            let interval_ms = start_n.and_then(|s| num(s, "interval")).unwrap_or(100.0);
            let vec_of = |n: Option<roxmltree::Node<'_, '_>>| {
                n.map_or((0.0, 0.0), |m| (num(m, "x").unwrap_or(0.0), num(m, "y").unwrap_or(0.0)))
            };
            let start_v = vec_of(start_n);
            let end_v = vec_of(end_n);
            let flip = seq.is_some_and(|s| {
                s.children()
                    .any(|n| n.has_tag_name("action") && n.text().map(str::trim) == Some("flip"))
            });
            // Sequence-end transitions. Keep the unconditional ones (`only="none"`
            // or absent); drop taskbar/window-scoped picks we can't satisfy.
            let next: Vec<(u32, u32)> = seq.map_or_else(Vec::new, |s| {
                s.children()
                    .filter(|n| n.has_tag_name("next"))
                    .filter(|n| matches!(n.attribute("only"), None | Some("none")))
                    .filter_map(|n| {
                        let p = n.attribute("probability").and_then(|v| v.parse().ok()).unwrap_or(100);
                        n.text().and_then(|t| t.trim().parse().ok()).map(|id| (p, id))
                    })
                    .collect()
            });
            // Border transitions, tagged with the edge they apply to.
            let edge_of = |only: Option<&str>| match only {
                Some("vertical") => Some(Edge::Side),
                Some("horizontal+") => Some(Edge::Top),
                Some("horizontal-") => Some(Edge::Bottom),
                None | Some("none") => Some(Edge::Any),
                _ => None, // window / taskbar: no such edge on a fullscreen surface
            };
            let border_next: Vec<(Edge, u32, u32)> = a
                .children()
                .find(|n| n.has_tag_name("border"))
                .map(|b| {
                    b.children()
                        .filter(|n| n.has_tag_name("next"))
                        .filter_map(|n| {
                            let edge = edge_of(n.attribute("only"))?;
                            let p = n.attribute("probability").and_then(|v| v.parse().ok()).unwrap_or(100);
                            let id = n.text().and_then(|t| t.trim().parse().ok())?;
                            Some((edge, p, id))
                        })
                        .collect()
                })
                .unwrap_or_default();
            let gravity_next = a
                .children()
                .find(|n| n.has_tag_name("gravity"))
                .and_then(|g| g.children().find(|n| n.has_tag_name("next")))
                .and_then(|n| n.text().and_then(|t| t.trim().parse().ok()));
            anims.insert(
                id,
                std::rc::Rc::new(Anim {
                    frames,
                    interval_ms: interval_ms.max(1.0),
                    start: start_v,
                    end: end_v,
                    flip,
                    next,
                    border_next,
                    gravity_next,
                }),
            );
            start = start.min(id);
        }
        if anims.is_empty() {
            return None;
        }
        let has_gravity = anims.values().any(|a| a.gravity_next.is_some());
        Some(Self {
            tilesx,
            tilesy,
            start: walk_id.unwrap_or(start),
            fall: fall_id,
            grabbed: drag_id.or(scream_id),
            eat: eat_id,
            anims,
            has_gravity,
            flyer: false, // set from the pet id by `load_pet`
        })
    }
}

/// A horizontal ledge the pet can stand on and walk along.
///
/// UI lines we hand in (the tab bar, …) become extra levels for a "multi-level
/// garden". `y` is the top of the ledge (pixels from the screen top); `x0..x1`
/// its span.
#[derive(Clone, Copy, Debug)]
pub struct Surface {
    pub y: f32,
    pub x0: f32,
    pub x1: f32,
}

/// The bounds the pet moves within for one tick.
///
/// Screen size + sprite-tile size, plus the standable `surfaces` (ledges). The
/// screen floor/walls/ceiling are handled implicitly; `surfaces` are the extra
/// mid-screen ledges.
pub struct World<'a> {
    pub w: f32,
    pub h: f32,
    pub tile_w: f32,
    pub tile_h: f32,
    pub surfaces: &'a [Surface],
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    (b - a).mul_add(t, a)
}

/// Which screen edges the pet is driving into this step (movement-gated, so
/// resting on the floor isn't a "hit"). Fed to [`Pet::pick_border`].
#[derive(Clone, Copy, Default)]
struct Hits {
    left: bool,
    right: bool,
    top: bool,
    bottom: bool,
}

/// A live pet instance walking the screen.
pub struct Pet {
    def: PetDef,
    /// Top-left position of the sprite, in screen pixels.
    x: f32,
    y: f32,
    /// -1 = facing/moving left, +1 = right.
    facing: i8,
    anim: u32,
    frame_i: usize,
    accum_ms: f32,
    rng: u32,
    /// While held by the mouse, physics is frozen and position is driven by the
    /// drag handler; releasing drops the pet (gravity takes over).
    dragging: bool,
    /// Grazing: the pet chews a grass tuft in place (the `eat` animation loops,
    /// no movement or transitions) until the overlay tells it to stop.
    eating: bool,
}

impl Pet {
    /// Spawn at the bottom-right, walking left (the eSheep default `spawn`).
    #[must_use]
    pub fn new(def: PetDef, screen_w: f32, screen_h: f32, tile_w: f32, tile_h: f32) -> Self {
        let start = def.start;
        Self {
            def,
            x: (screen_w - tile_w).max(0.0),
            y: (screen_h - tile_h).max(0.0),
            facing: -1,
            anim: start,
            frame_i: 0,
            accum_ms: 0.0,
            rng: 0x2545_F491,
            dragging: false,
            eating: false,
        }
    }

    /// Reset to the spawn state — bottom, facing left, walking. Upstream's
    /// `id < 0 → spawn` fallback (`FormPet.SetNewAnimation`) when an animation
    /// dead-ends; mirrors [`Pet::new`]'s spawn using the live screen size.
    fn respawn(&mut self, world: &World) {
        self.x = (world.w - world.tile_w).max(0.0);
        self.y = (world.h - world.tile_h).max(0.0);
        self.facing = -1;
        self.anim = self.def.start;
        self.frame_i = 0;
        self.accum_ms = 0.0;
        self.eating = false;
    }

    /// Cheap LCG so `next`-probability picks don't need the `rand` crate.
    const fn rand_100(&mut self) -> u32 {
        self.rng = self.rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (self.rng >> 24) % 100
    }

    /// Advance the animation by `dt_ms`, moving across floor/walls/ceiling/ledges
    /// and applying gravity. No-op while the pet is being dragged.
    pub fn tick(&mut self, dt_ms: f32, world: &World) {
        if self.dragging {
            return;
        }
        // Grazing: chew in place — cycle the eat frames, no movement/transitions.
        if self.eating {
            if let Some(cur) = self.def.anims.get(&self.anim) {
                self.accum_ms += dt_ms;
                let mut steps = 0;
                while self.accum_ms >= cur.interval_ms && steps < 8 {
                    self.accum_ms -= cur.interval_ms;
                    steps += 1;
                    self.frame_i = (self.frame_i + 1) % cur.frames.len().max(1);
                }
            }
            return;
        }
        let Some(mut cur) = self.def.anims.get(&self.anim).cloned() else {
            return;
        };
        self.accum_ms += dt_ms;
        // Cap the catch-up so a stalled UI thread doesn't teleport the pet.
        let mut steps = 0;
        while self.accum_ms >= cur.interval_ms && steps < 8 {
            self.accum_ms -= cur.interval_ms;
            steps += 1;
            self.step(&cur, world);
            if let Some(next) = self.def.anims.get(&self.anim) {
                cur = next.clone();
            } else {
                break;
            }
        }
    }

    fn step(&mut self, cur: &Anim, world: &World) {
        let (tw, th) = (world.tile_w, world.tile_h);
        let n = cur.frames.len().max(1);
        let t = if n > 1 {
            self.frame_i as f32 / (n - 1) as f32
        } else {
            0.0
        };
        // `x` is mirrored by facing; `y` (gravity) never is.
        let mult = if self.facing < 0 { 1.0 } else { -1.0 };
        let dx = lerp(cur.start.0, cur.end.0, t) * mult;
        let dy = lerp(cur.start.1, cur.end.1, t);

        // Was the pet already resting on the floor/a ledge before this step? A
        // falling animation keeps dy > 0 after touching down, so without this the
        // floor would re-fire a "landing" every step and the pet would loop the
        // fall sequence forever (stuck upright). Only count a bottom hit when it
        // actually descended onto the floor this step.
        let was_airborne = self.airborne(world);

        // A downward-moving (fall/`fall_face`) animation that's resting on the
        // ground has, by definition, landed — its motion can't descend further.
        // The fall graph can wander back into a fall state on the floor (e.g.
        // after a throw), where no bottom-hit fires because we're already down;
        // snap it back to walking so it doesn't get stuck face-down. Airborne
        // falls are untouched — those land via the bottom border below.
        if !was_airborne && dy > 0.0 {
            self.anim = self.def.start;
            self.frame_i = 0;
            return;
        }

        let old_feet = self.y + th;
        let mut nx = self.x + dx;
        let mut ny = self.y + dy;

        // Edges the pet is *moving into* this step (before clamping). Gated on
        // direction so walking along the floor (dy == 0) isn't a bottom hit — the
        // floor/ceiling are surfaces, not turn-around borders.
        let hits = Hits {
            left: dx < 0.0 && nx <= 0.0,
            right: dx > 0.0 && nx + tw >= world.w,
            top: dy < 0.0 && ny <= 0.0,
            bottom: was_airborne && dy > 0.0 && ny + th >= world.h,
        };
        nx = nx.clamp(0.0, (world.w - tw).max(0.0));
        ny = ny.clamp(0.0, (world.h - th).max(0.0));

        // Landing on a mid-screen ledge: while descending, the feet cross a
        // surface top within its span. Snap onto the highest such ledge.
        if was_airborne && dy > 0.0 {
            let cx = tw.mul_add(0.5, nx);
            let new_feet = ny + th;
            let landed = world
                .surfaces
                .iter()
                .filter(|s| cx >= s.x0 && cx <= s.x1 && s.y >= old_feet - 1.0 && s.y <= new_feet + 0.5)
                .map(|s| s.y)
                .fold(None, |acc: Option<f32>, y| Some(acc.map_or(y, |a| a.min(y))));
            if let Some(sy) = landed {
                self.x = nx;
                self.y = sy - th;
                let bottom = Hits {
                    bottom: true,
                    ..Hits::default()
                };
                self.anim = self.pick_border(cur, bottom).unwrap_or(self.def.start);
                self.frame_i = 0;
                return;
            }
        }

        self.x = nx;
        self.y = ny;

        // Screen-edge transition (climb a wall, walk the ceiling, land on floor).
        if let Some(id) = self.pick_border(cur, hits) {
            self.anim = id;
            self.frame_i = 0;
            return;
        }

        // Gravity, for a pet caught in mid-air (walked off a ledge, or dropped).
        if self.airborne(world) {
            if self.def.flyer {
                // The owl is a bird: it flies rather than falls. No downward pull —
                // its walk animation carries it horizontally and a gentle random
                // bob gives it lift/dip, so it drifts around the air. Falls through
                // to the frame advance below so it keeps flapping. It only lands if
                // the bob happens to bring it onto a surface (then `airborne` is
                // false and it walks normally).
                let bob = (self.rand_100() as f32 - 50.0) * 0.06; // ≈ ±3 px, ~neutral
                self.y = (self.y + bob).clamp(0.0, (world.h - th).max(0.0));
            } else if let Some(g) = cur.gravity_next {
                // The pet has its own fall animation — play it.
                self.anim = g;
                self.frame_i = 0;
                return;
            } else if !self.def.has_gravity {
                // A pet with no fall animation at all (the ham-ham). Drift straight
                // down to the nearest surface below so a dropped pet doesn't
                // hover, then resume walking on landing.
                const FALL: f32 = 9.0;
                let feet = self.y + th;
                let cx = tw.mul_add(0.5, self.x);
                let target = world
                    .surfaces
                    .iter()
                    .filter(|s| cx >= s.x0 && cx <= s.x1 && s.y >= feet - 0.5)
                    .map(|s| s.y)
                    .fold(world.h, f32::min);
                let new_feet = (feet + FALL).min(target);
                self.y = new_feet - th;
                if (new_feet - target).abs() < 0.5 {
                    self.anim = self.def.start;
                    self.frame_i = 0;
                }
                return;
            }
            // Otherwise it's an intentionally-aerial animation (climb/ceiling/
            // jump) on a gravity-capable pet — leave it be.
        }

        // Advance the frame; at the end, apply flip + pick the next animation.
        self.frame_i += 1;
        if self.frame_i >= n {
            self.frame_i = 0;
            if cur.flip {
                self.facing = -self.facing;
            }
            // A sequence with no `<next>` is a dead end — desktopPet's death /
            // kill / effect animations (alien_kill, blank_die, …) whose empty next
            // means "end of life". Upstream never freezes on these: FormPet.cs
            // `SetNewAnimation` does `if (id < 0) // no animation found, spawn!` —
            // it respawns the pet. We match that: respawn at the start point so a
            // sheep can never get stuck (e.g. frozen forever at the top of screen).
            let grounded = !self.airborne(world);
            if let Some(next) = self.pick_next(cur, grounded) {
                self.anim = next;
            } else {
                self.respawn(world);
            }
        }
    }

    /// True when the pet's feet rest on neither the screen floor nor a ledge.
    fn airborne(&self, world: &World) -> bool {
        let feet = self.y + world.tile_h;
        if feet >= world.h - 1.5 {
            return false;
        }
        let cx = world.tile_w.mul_add(0.5, self.x);
        !world
            .surfaces
            .iter()
            .any(|s| cx >= s.x0 && cx <= s.x1 && (s.y - feet).abs() <= 1.5)
    }

    /// Pick a border transition among those matching a hit edge, weighted by
    /// their probabilities (fall back to the first candidate).
    fn pick_border(&mut self, cur: &Anim, hits: Hits) -> Option<u32> {
        let matches = |edge: Edge| match edge {
            Edge::Any => hits.left || hits.right || hits.top || hits.bottom,
            Edge::Side => hits.left || hits.right,
            Edge::Top => hits.top,
            Edge::Bottom => hits.bottom,
        };
        let mut first = None;
        for &(edge, prob, id) in &cur.border_next {
            if !matches(edge) {
                continue;
            }
            first.get_or_insert(id);
            if self.rand_100() < prob {
                return Some(id);
            }
        }
        first
    }

    /// Choose the animation to play when `cur`'s sequence ends.
    ///
    /// The data-driven path just weights `cur`'s `<next>` entries by their
    /// probabilities. But the eSheep sheet makes the walk sequence branch into
    /// a special/idle animation (sit, look around, …) fairly often, which reads
    /// as the pet "performing" constantly. When a grounded pet finishes a walk,
    /// we short-circuit and keep walking most of the time (`WALK_STICK`), so
    /// specials become the exception and the pet spends its time roaming (and
    /// thus reaching grass to graze). Aerial sequences (climb/fall/ceiling) are
    /// left fully data-driven so wall-climbing and gravity graphs still resolve.
    fn pick_next(&mut self, cur: &Anim, grounded: bool) -> Option<u32> {
        /// Chance a grounded pet just walks again instead of rolling a special.
        const WALK_STICK: u32 = 82;
        if grounded && self.anim == self.def.start && self.rand_100() < WALK_STICK {
            return Some(self.def.start);
        }
        for &(prob, id) in &cur.next {
            if self.rand_100() < prob {
                return Some(id);
            }
        }
        cur.next.first().map(|&(_, id)| id)
    }

    /// Begin a drag: freeze physics until [`Pet::release`].
    pub const fn grab(&mut self) {
        self.dragging = true;
        self.eating = false;
        // Look astonished while held (desktopPet's `drag`, or `scream`). Physics
        // is frozen during the drag, so this just shows the surprised frame.
        if let Some(g) = self.def.grabbed {
            self.anim = g;
            self.frame_i = 0;
        }
    }

    /// Whether this pet grazes (has an `eat` animation — the sheep).
    #[must_use]
    pub const fn can_graze(&self) -> bool {
        self.def.eat.is_some()
    }

    /// Whether it's currently walking (its start animation) and free to react —
    /// so grazing only kicks in from a normal stroll, not mid-climb/fall/drag.
    #[must_use]
    pub const fn is_walking(&self) -> bool {
        self.anim == self.def.start && !self.eating && !self.dragging
    }

    #[must_use]
    pub const fn is_grazing(&self) -> bool {
        self.eating
    }

    /// The pet's feet: sprite-centre x and bottom y — where it stands on a ledge.
    #[must_use]
    pub fn feet(&self, tile_w: f32, tile_h: f32) -> (f32, f32) {
        (tile_w.mul_add(0.5, self.x), self.y + tile_h)
    }

    /// Start nibbling a grass tuft in place (plays the `eat` animation on loop).
    pub const fn start_grazing(&mut self) {
        if let Some(e) = self.def.eat {
            self.eating = true;
            self.anim = e;
            self.frame_i = 0;
        }
    }

    /// Stop grazing and walk on.
    pub const fn stop_grazing(&mut self) {
        self.eating = false;
        self.anim = self.def.start;
        self.frame_i = 0;
    }

    /// Move the pet to a top-left position (used while dragging).
    pub const fn set_pos(&mut self, x: f32, y: f32) {
        self.x = x;
        self.y = y;
    }

    /// Release a drag. Switch into the `fall` animation (or the start animation
    /// for pets without one) so the pet drops under gravity from wherever it was
    /// let go — instead of resuming the (e.g. wall-climbing) animation it was
    /// grabbed in and "walking a wall" in mid-air. On the floor, the fall resolves
    /// straight back to walking (see the grounded-fall check in `step`).
    pub fn release(&mut self) {
        self.dragging = false;
        self.eating = false;
        self.anim = self.def.fall.unwrap_or(self.def.start);
        self.frame_i = 0;
    }

    #[must_use]
    pub const fn is_dragging(&self) -> bool {
        self.dragging
    }

    /// The `(col, row)` of the active sprite tile in the sheet.
    #[must_use]
    pub fn current_tile(&self) -> (u32, u32) {
        let frame = self
            .def
            .anims
            .get(&self.anim)
            .and_then(|a| a.frames.get(self.frame_i).copied())
            .unwrap_or(0);
        (frame % self.def.tilesx, frame / self.def.tilesx)
    }

    /// Top-left screen position of the sprite, in pixels.
    #[must_use]
    pub const fn pos(&self) -> (f32, f32) {
        (self.x, self.y)
    }

    /// Whether the sprite should be drawn mirrored (facing right).
    #[must_use]
    pub const fn flipped(&self) -> bool {
        self.facing > 0
    }
}

/// A pet loaded and ready to render: animation model + its sprite sheets.
pub struct LoadedPet {
    pub def: PetDef,
    /// The sprite-sheet PNG bytes — an alpha-keyed, palette-quantized sheet baked
    /// at build time (see `tools/bake-pets`), so feed straight to
    /// `gpui::Image::from_bytes` with no runtime decode/keying.
    pub png: Vec<u8>,
    /// The same sheet with every tile mirrored horizontally in place, drawn when
    /// the pet faces the other way ([`Pet::flipped`]) — gpui can't mirror a raster
    /// element, so we ship both orientations.
    pub png_flip: Vec<u8>,
    pub sheet_w: f32,
    pub sheet_h: f32,
}

/// The pet asset directories, most-preferred first: the installed location, then
/// `./assets/pets/` for `cargo run` dev builds.
fn pet_dirs() -> [PathBuf; 2] {
    [
        PathBuf::from("/usr/share/tab-atelier/pets"),
        PathBuf::from("assets/pets"),
    ]
}

/// List the available pets as `(id, display_name)` — the XML file stem and its
/// `<petname>`. Reads only the name (a cheap substring scan), not the whole 1 MB
/// document, so building the summon menu is fast.
#[must_use]
pub fn list_pets() -> Vec<(String, String)> {
    for dir in pet_dirs() {
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut out = Vec::new();
        for entry in rd.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "xml") {
                continue;
            }
            let Some(id) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
                continue;
            };
            let name = std::fs::read_to_string(&path)
                .ok()
                .and_then(|x| extract_tag(&x, "petname"))
                .unwrap_or_else(|| id.clone());
            out.push((id, name));
        }
        if !out.is_empty() {
            out.sort();
            return out;
        }
    }
    Vec::new()
}

/// Load a pet by id (asset file stem).
///
/// Parses the `<id>.xml` animations and reads the sibling baked `<id>.png` sprite
/// sheet. `None` on any failure. The `id` is validated to reject path traversal
/// (only `[A-Za-z0-9_-]`).
#[must_use]
pub fn load_pet(id: &str) -> Option<LoadedPet> {
    let safe = id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if !safe || id.is_empty() {
        return None;
    }
    let dir = pet_dirs().into_iter().find(|d| d.join(format!("{id}.xml")).is_file())?;
    let xml = std::fs::read_to_string(dir.join(format!("{id}.xml"))).ok()?;
    let mut def = PetDef::parse(&xml)?;
    // The owl is a bird — it flies instead of falling when airborne.
    def.flyer = id.contains("owl");
    let png = std::fs::read(dir.join(format!("{id}.png"))).ok()?;
    let (sheet_w, sheet_h) = png_dims(&png)?;
    // The mirrored sheet is optional — fall back to the normal one so a pet with
    // no baked flip still renders (just un-mirrored when facing the other way).
    let png_flip = std::fs::read(dir.join(format!("{id}.flip.png"))).unwrap_or_else(|_| png.clone());
    Some(LoadedPet {
        def,
        png,
        png_flip,
        sheet_w,
        sheet_h,
    })
}

/// Inner text of the first `<tag>…</tag>`, trimmed. Used to pull the `<petname>`
/// for the summon menu without a full XML parse.
fn extract_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let s = xml.find(&open)? + open.len();
    let e = xml[s..].find(&close)? + s;
    Some(xml[s..e].trim().to_string())
}

/// Read a PNG's pixel `(width, height)` straight from its IHDR — no full decode,
/// so we can compute the sprite-tile size (`width/tilesx`, `height/tilesy`).
#[must_use]
fn png_dims(bytes: &[u8]) -> Option<(f32, f32)> {
    if bytes.len() < 24 || &bytes[0..8] != b"\x89PNG\r\n\x1a\n" {
        return None;
    }
    let w = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let h = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    (w > 0 && h > 0).then_some((w as f32, h as f32))
}

/// One live pet: its movement state plus its two sprite sheets and geometry.
struct LivePet {
    pet: Pet,
    sheet: Arc<gpui::Image>,
    sheet_flip: Arc<gpui::Image>,
    sheet_wh: (f32, f32),
    tile_wh: (f32, f32),
}

/// A tuft of grass on a ledge that grazing sheep eat down. `amount` is fullness
/// in `0..1`; it shrinks as eaten and the tuft is removed at 0.
#[derive(Clone, Copy, Debug)]
struct Grass {
    x: f32,
    y: f32,
    amount: f32,
}

/// The desktop screen-mates: a herd of pets and the grass they graze.
///
/// Owns the pets, the grass tufts on the tab lines, and the gpui glue to summon,
/// animate, draw, drag, grow, and auto-spawn them. `app.rs` keeps a single
/// `PetOverlay` field and a few delegating calls.
pub struct PetOverlay {
    pets: Vec<LivePet>,
    last: Instant,
    /// While dragging: `(pet index, grab offset mouse - top_left)`.
    drag: Option<(usize, (f32, f32))>,
    /// Ledges (one per tab, its top edge) collected each paint by the measuring
    /// canvases — the pets walk them and grass grows on them.
    ledges: Rc<RefCell<Vec<Surface>>>,
    /// Reused buffer for `advance`'s per-frame ledge snapshot (the 20 fps
    /// tick used to clone the Vec every frame).
    ledges_scratch: Vec<Surface>,
    /// Cached grazer (sheep) pet ids — see `spawn_grazer`.
    grazer_ids: Option<Vec<String>>,
    /// Grass tufts on the ledges; grow over time, eaten by grazing sheep.
    grass: Vec<Grass>,
    grow_accum: f32,
    spawn_accum: f32,
    /// How many pets the user asked for (summon clicks). The auto-spawning
    /// grazers top the herd up toward this but never past it — so the herd can't
    /// run away on its own.
    asked: usize,
    rng: u32,
}

impl Default for PetOverlay {
    fn default() -> Self {
        Self {
            pets: Vec::new(),
            last: Instant::now(),
            drag: None,
            ledges: Rc::new(RefCell::new(Vec::new())),
            ledges_scratch: Vec::new(),
            grazer_ids: None,
            grass: Vec::new(),
            grow_accum: 0.0,
            spawn_accum: 0.0,
            asked: 0,
            rng: 0x9E37_79B9,
        }
    }
}

impl PetOverlay {
    /// Whether any pet is currently on screen.
    #[must_use]
    pub const fn is_active(&self) -> bool {
        !self.pets.is_empty()
    }

    /// How many pets are on screen — drives the "dismiss all" menu entry.
    #[must_use]
    pub const fn count(&self) -> usize {
        self.pets.len()
    }

    /// Cheap LCG for random species / grass positions.
    const fn rand_u32(&mut self) -> u32 {
        self.rng = self.rng.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        self.rng
    }

    /// Load a pet by id and build a `LivePet` at the spawn point.
    fn build(id: &str, screen_w: f32, screen_h: f32) -> Option<LivePet> {
        let loaded = load_pet(id)?;
        let (sw, sh) = (loaded.sheet_w, loaded.sheet_h);
        let (tw, th) = (sw / loaded.def.tilesx as f32, sh / loaded.def.tilesy as f32);
        Some(LivePet {
            pet: Pet::new(loaded.def, screen_w, screen_h, tw, th),
            sheet: Arc::new(gpui::Image::from_bytes(gpui::ImageFormat::Png, loaded.png)),
            sheet_flip: Arc::new(gpui::Image::from_bytes(gpui::ImageFormat::Png, loaded.png_flip)),
            sheet_wh: (sw, sh),
            tile_wh: (tw, th),
        })
    }

    /// Summon one more random pet — each click raises the herd's target size and
    /// adds a pet. Auto-spawning never pushes the total past this.
    pub fn summon(&mut self, screen_w: f32, screen_h: f32) {
        let choices = list_pets();
        if choices.is_empty() {
            return;
        }
        let idx = self.rand_u32() as usize % choices.len();
        if let Some(lp) = Self::build(&choices[idx].0, screen_w, screen_h) {
            self.pets.push(lp);
            self.asked += 1;
        }
    }

    /// Auto-spawn a grazing sheep to top the herd up toward the asked count. Hard
    /// cap: never spawn past `asked`, so the grazing herd can't run away.
    fn spawn_grazer(&mut self, screen_w: f32, screen_h: f32) {
        if self.pets.len() >= self.asked {
            return;
        }
        // The installed pet set is static for the process lifetime, and
        // `list_pets` reads every pet XML in full — cache the sheep ids
        // instead of re-scanning the directory every 6 s spawn tick.
        if self.grazer_ids.is_none() {
            self.grazer_ids = Some(
                list_pets()
                    .into_iter()
                    .map(|(id, _)| id)
                    .filter(|id| id.contains("sheep"))
                    .collect(),
            );
        }
        let len = self.grazer_ids.as_ref().map_or(0, Vec::len);
        if len == 0 {
            return;
        }
        let idx = self.rand_u32() as usize % len;
        let Some(id) = self.grazer_ids.as_ref().and_then(|v| v.get(idx)).cloned() else {
            return;
        };
        if let Some(lp) = Self::build(&id, screen_w, screen_h) {
            self.pets.push(lp);
        }
    }

    /// Remove every pet and all grass, and reset the asked count.
    pub fn dismiss_all(&mut self) {
        self.pets.clear();
        self.grass.clear();
        self.asked = 0;
        self.drag = None;
    }

    /// A measuring canvas for tab `index`'s top edge, appended to the ledge list.
    /// The first tab clears last frame's list before anyone appends (canvases
    /// paint in child order). Absolute + full-size so it measures the tab without
    /// disturbing layout or stealing mouse events (it has no hitbox).
    #[must_use]
    pub fn tab_ledge_canvas(&self, index: usize) -> impl IntoElement {
        let ledges = self.ledges.clone();
        canvas(
            move |bounds, _, _| {
                let mut v = ledges.borrow_mut();
                if index == 0 {
                    v.clear();
                }
                let x0 = f32::from(bounds.origin.x);
                v.push(Surface {
                    y: f32::from(bounds.origin.y),
                    x0,
                    x1: x0 + f32::from(bounds.size.width),
                });
            },
            |_, (), _, _| {},
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full()
    }

    /// Grow grass on the tab-line ledges, auto-spawn grazers while there's grass,
    /// run grazing, and tick every pet. One real-time frame of the ecosystem.
    fn advance(&mut self, dt_ms: f32, screen_w: f32, screen_h: f32) {
        const GROW_MS: f32 = 3500.0; // a new grass tuft this often
        const GRASS_MAX: usize = 60; // soft cap on tufts (not pets)
        const SPAWN_MS: f32 = 6000.0; // a new grazing sheep this often, while grass lasts
        const EAT_RATE: f32 = 0.4; // fullness eaten per second

        // Snapshot the ledges (Copy) so we can borrow `self` mutably below,
        // reusing the scratch buffer instead of a per-frame clone.
        let mut ledges = std::mem::take(&mut self.ledges_scratch);
        ledges.clear();
        ledges.extend_from_slice(&self.ledges.borrow());

        // 1. Sprout a grass tuft now and then at a random spot on a random ledge.
        self.grow_accum += dt_ms;
        if self.grow_accum >= GROW_MS {
            self.grow_accum = 0.0;
            if !ledges.is_empty() && self.grass.len() < GRASS_MAX {
                let s = ledges[self.rand_u32() as usize % ledges.len()];
                // Random fraction in 0..1 from the top 24 bits (exact in f32).
                let f = f64::from(self.rand_u32() >> 8) as f32 / 16_777_216.0;
                let x = (s.x1 - s.x0).max(1.0).mul_add(f, s.x0);
                self.grass.push(Grass { x, y: s.y, amount: 1.0 });
            }
        }

        // 2. While there's grass, a new sheep wanders in every so often (no cap;
        //    self-limited by grass + rate). No grass ⇒ the herd stops growing.
        if self.grass.is_empty() {
            self.spawn_accum = 0.0;
        } else {
            self.spawn_accum += dt_ms;
            if self.spawn_accum >= SPAWN_MS {
                self.spawn_accum = 0.0;
                self.spawn_grazer(screen_w, screen_h);
            }
        }

        // 3. Grazing + movement per pet.
        for i in 0..self.pets.len() {
            let (tw, th) = self.pets[i].tile_wh;
            if self.pets[i].pet.can_graze() {
                let (cx, feet) = self.pets[i].pet.feet(tw, th);
                let over = self
                    .grass
                    .iter()
                    .position(|g| g.amount > 0.0 && (g.y - feet).abs() < 3.0 && (g.x - cx).abs() < tw * 0.5);
                match (self.pets[i].pet.is_grazing(), over) {
                    (false, Some(_)) if self.pets[i].pet.is_walking() => self.pets[i].pet.start_grazing(),
                    (true, Some(gi)) => {
                        self.grass[gi].amount -= dt_ms / 1000.0 * EAT_RATE;
                        if self.grass[gi].amount <= 0.0 {
                            self.grass.remove(gi);
                            self.pets[i].pet.stop_grazing();
                        }
                    }
                    (true, None) => self.pets[i].pet.stop_grazing(),
                    _ => {}
                }
            }
            let world = World {
                w: screen_w,
                h: screen_h,
                tile_w: tw,
                tile_h: th,
                surfaces: &ledges,
            };
            self.pets[i].pet.tick(dt_ms, &world);
        }

        // Hand the snapshot buffer back for the next frame.
        self.ledges_scratch = ledges;
    }

    /// A little green tuft drawn on a ledge; its height tracks how much is left.
    fn grass_tuft(g: Grass) -> gpui::AnyElement {
        let h = (g.amount * 9.0).clamp(2.0, 9.0);
        let green = gpui::rgb(0x4f_9d_3a);
        let blade = move |dx: f32, bh: f32| {
            div()
                .absolute()
                .left(px(dx))
                .bottom(px(0.0))
                .w(px(2.0))
                .h(px(bh))
                .rounded_full()
                .bg(green)
        };
        div()
            .absolute()
            .left(px(g.x - 5.0))
            .top(px(g.y - h))
            .w(px(10.0))
            .h(px(h))
            .child(blade(1.0, h * 0.75))
            .child(blade(4.0, h))
            .child(blade(7.0, h * 0.65))
            .into_any_element()
    }

    /// The clickable sprite for one pet (index `i`), facing the way it moves.
    fn pet_sprite<V: 'static>(
        i: usize,
        lp: &LivePet,
        cx: &mut Context<V>,
        access: fn(&mut V) -> &mut Self,
    ) -> gpui::AnyElement {
        let (tw, th) = lp.tile_wh;
        let (sw, sh) = lp.sheet_wh;
        let (col, row) = lp.pet.current_tile();
        let (x, y) = lp.pet.pos();
        let dragging = lp.pet.is_dragging();
        let sheet = if lp.pet.flipped() { &lp.sheet_flip } else { &lp.sheet };
        div()
            .absolute()
            .left(px(x))
            .top(px(y))
            .w(px(tw))
            .h(px(th))
            .overflow_hidden()
            .occlude()
            .cursor(if dragging {
                gpui::CursorStyle::ClosedHand
            } else {
                gpui::CursorStyle::OpenHand
            })
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, ev: &gpui::MouseDownEvent, _window, cx| {
                    let o = access(this);
                    if let Some(lp) = o.pets.get_mut(i) {
                        let (px_, py_) = lp.pet.pos();
                        lp.pet.grab();
                        o.drag = Some((i, (f32::from(ev.position.x) - px_, f32::from(ev.position.y) - py_)));
                        cx.notify();
                    }
                }),
            )
            .child(
                img(gpui::ImageSource::Image(sheet.clone()))
                    .absolute()
                    .left(px(-(col as f32) * tw))
                    .top(px(-(row as f32) * th))
                    .w(px(sw))
                    .h(px(sh)),
            )
            .into_any_element()
    }

    /// Advance the herd + grass and build the overlay: grass tufts, every pet's
    /// sprite, and a full-window drag catcher for whichever pet is held. `visible`
    /// freezes it when the window is hidden; `access` maps the render entity back
    /// to this overlay for the drag listeners. `None` when nothing is on screen.
    pub fn render<V: 'static>(
        &mut self,
        visible: bool,
        screen_w: f32,
        screen_h: f32,
        cx: &mut Context<V>,
        access: fn(&mut V) -> &mut Self,
    ) -> Option<gpui::AnyElement> {
        if self.pets.is_empty() && self.grass.is_empty() {
            return None;
        }
        // Frame timing: real dt so speeds are display-rate independent. Frozen
        // while hidden — no point animating off-screen.
        let now = Instant::now();
        let dt_ms = (now.saturating_duration_since(self.last).as_secs_f32() * 1000.0).min(200.0);
        self.last = now;
        if visible {
            self.advance(dt_ms, screen_w, screen_h);
        }

        let mut wrap = div().absolute().top_0().left_0().w(px(screen_w)).h(px(screen_h));
        for g in &self.grass {
            wrap = wrap.child(Self::grass_tuft(*g));
        }
        for (i, lp) in self.pets.iter().enumerate() {
            wrap = wrap.child(Self::pet_sprite(i, lp, cx, access));
        }

        // Full-window catcher for the held pet, so the pointer needn't stay inside
        // the tiny sprite box while dragging.
        if let Some((di, _)) = self.drag {
            wrap = wrap.child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .w(px(screen_w))
                    .h(px(screen_h))
                    .occlude()
                    .cursor(gpui::CursorStyle::ClosedHand)
                    .on_mouse_move(cx.listener(move |this, ev: &gpui::MouseMoveEvent, _window, cx| {
                        let o = access(this);
                        if let (Some((idx, off)), Some(lp)) = (o.drag, o.pets.get_mut(di)) {
                            let (tw, th) = lp.tile_wh;
                            let nx = (f32::from(ev.position.x) - off.0).clamp(0.0, (screen_w - tw).max(0.0));
                            let ny = (f32::from(ev.position.y) - off.1).clamp(0.0, (screen_h - th).max(0.0));
                            debug_assert_eq!(idx, di);
                            lp.pet.set_pos(nx, ny);
                            cx.notify();
                        }
                    }))
                    .on_mouse_up(
                        gpui::MouseButton::Left,
                        cx.listener(move |this, _ev: &gpui::MouseUpEvent, _window, cx| {
                            let o = access(this);
                            if let Some(lp) = o.pets.get_mut(di) {
                                lp.pet.release();
                            }
                            o.drag = None;
                            cx.notify();
                        }),
                    ),
            );
        }
        Some(wrap.into_any_element())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const XML: &str = include_str!("../assets/pets/owl.xml");

    #[test]
    fn png_dims_reads_ihdr() {
        let png = std::fs::read("assets/pets/owl.png").unwrap();
        assert_eq!(png_dims(&png), Some((784.0, 588.0)));
        assert_eq!(png_dims(b"not a png"), None);
    }

    #[test]
    fn extract_tag_pulls_the_petname() {
        assert!(extract_tag(XML, "petname").is_some());
        assert_eq!(extract_tag(XML, "no-such-tag"), None);
    }

    #[test]
    fn load_pet_reads_def_and_baked_sheet() {
        let owl = load_pet("owl").expect("load owl");
        assert_eq!((owl.sheet_w, owl.sheet_h), (784.0, 588.0));
        assert_eq!(&owl.png[0..8], b"\x89PNG\r\n\x1a\n", "baked sibling PNG");
        assert!(load_pet("../etc/passwd").is_none(), "rejects path traversal");
        assert!(load_pet("").is_none());
    }

    /// A full-screen world with no extra ledges, for the movement tests.
    fn bare_world(w: f32, h: f32, tile: f32) -> World<'static> {
        World {
            w,
            h,
            tile_w: tile,
            tile_h: tile,
            surfaces: &[],
        }
    }

    #[test]
    fn parses_owl_geometry_and_walk() {
        let def = PetDef::parse(XML).expect("parse");
        assert_eq!((def.tilesx, def.tilesy), (16, 12));
        // walk (id 2) is the start animation and has 12 frames.
        assert_eq!(def.start, 2);
        assert_eq!(def.anims[&2].frames.len(), 12);
        assert!(def.anims[&2].start.0.abs() > 0.0, "walk moves horizontally");
        assert!(
            def.anims[&2].border_next.iter().any(|&(_, _, id)| id == 3),
            "walk turns at the border"
        );
        // rotate1 (id 3) flips the pet and doesn't move.
        let rot = &def.anims[&3];
        assert!(rot.flip);
        assert!(
            rot.start.0.abs() < f32::EPSILON && rot.end.0.abs() < f32::EPSILON,
            "rotate doesn't move"
        );
    }

    #[test]
    fn current_tile_maps_frame_to_grid() {
        let def = PetDef::parse(XML).unwrap();
        let pet = Pet::new(def, 800.0, 600.0, 49.0, 49.0);
        // frame 0 -> col 0, row 0
        assert_eq!(pet.current_tile(), (0, 0));
    }

    #[test]
    fn walk_moves_left_then_turns_at_the_border() {
        let def = PetDef::parse(XML).unwrap();
        let mut pet = Pet::new(def, 200.0, 100.0, 49.0, 49.0);
        assert!((pet.pos().0 - (200.0 - 49.0)).abs() < 0.01);
        assert!(!pet.flipped(), "spawns facing left");
        // Walk to the left edge, then confirm it turns around (flips to face
        // right) — the pet oscillates, so watch for the event, not a fixed
        // state after N steps.
        let (mut hit_left, mut turned) = (false, false);
        for _ in 0..120 {
            pet.tick(100.0, &bare_world(200.0, 100.0, 49.0));
            if pet.pos().0 <= 1.0 {
                hit_left = true;
            }
            if hit_left && pet.flipped() {
                turned = true;
                break;
            }
        }
        assert!(hit_left, "reached the left edge");
        assert!(turned, "turned around (flipped) after hitting the border");
    }

    #[test]
    fn airborne_detects_floor_and_ledges() {
        let def = PetDef::parse(XML).unwrap();
        let mut pet = Pet::new(def, 400.0, 300.0, 49.0, 49.0);
        let ledge = [Surface {
            y: 150.0,
            x0: 0.0,
            x1: 400.0,
        }];
        let world = World {
            w: 400.0,
            h: 300.0,
            tile_w: 49.0,
            tile_h: 49.0,
            surfaces: &ledge,
        };
        // Spawns with feet on the floor.
        assert!(!pet.airborne(&world), "on the floor");
        // Up in the air, above the ledge — nothing under the feet.
        pet.set_pos(100.0, 1.0);
        assert!(pet.airborne(&world));
        // Feet exactly on the ledge top.
        pet.set_pos(100.0, 150.0 - 49.0);
        assert!(!pet.airborne(&world), "resting on the ledge");
    }

    #[test]
    fn owl_walks_the_full_width() {
        // Regression guard for "the pet only shuffles in a small square": the owl
        // must reach both side walls, i.e. roam the entire width. Deterministic
        // (the LCG is fixed-seeded).
        let lp = load_pet("owl").expect("owl assets");
        let (w, h) = (1000.0f32, 700.0f32);
        let (tw, th) = (lp.sheet_w / lp.def.tilesx as f32, lp.sheet_h / lp.def.tilesy as f32);
        let mut pet = Pet::new(lp.def, w, h, tw, th);
        let (mut minx, mut maxx) = (w, 0.0f32);
        for _ in 0..4000 {
            pet.tick(
                30.0,
                &World {
                    w,
                    h,
                    tile_w: tw,
                    tile_h: th,
                    surfaces: &[],
                },
            );
            let x = pet.pos().0;
            minx = minx.min(x);
            maxx = maxx.max(x);
        }
        assert!(minx <= 1.0, "reaches the left wall (minx={minx})");
        assert!(maxx >= w - tw - 1.0, "reaches the right wall (maxx={maxx})");
    }

    #[test]
    fn dropped_sheep_lands_on_the_floor() {
        // Regression: a fall must resolve to the floor (not loop the fall sequence
        // "stuck upright"). The sheep has gravity, so a dropped one settles down.
        let lp = load_pet("blue_sheep").expect("sheep assets");
        let (w, h) = (1000.0f32, 700.0f32);
        let (tw, th) = (lp.sheet_w / lp.def.tilesx as f32, lp.sheet_h / lp.def.tilesy as f32);
        let floor = h - th;
        let mut pet = Pet::new(lp.def, w, h, tw, th);
        pet.grab();
        pet.set_pos(w * 0.5, 30.0);
        pet.release();
        for _ in 0..800 {
            pet.tick(30.0, &bare_world(w, h, tw));
        }
        assert!(
            (pet.pos().1 - floor).abs() < 2.0,
            "sheep settled on the floor (y={})",
            pet.pos().1
        );
    }

    #[test]
    fn dropped_owl_flies_instead_of_falling() {
        // The owl is a flyer: dropped mid-air it stays aloft (flying), it does not
        // plummet to the floor like the sheep.
        let lp = load_pet("owl").expect("owl assets");
        let (w, h) = (1000.0f32, 700.0f32);
        let (tw, th) = (lp.sheet_w / lp.def.tilesx as f32, lp.sheet_h / lp.def.tilesy as f32);
        let mut pet = Pet::new(lp.def, w, h, tw, th);
        pet.grab();
        pet.set_pos(w * 0.5, 40.0); // released near the top
        pet.release();
        for _ in 0..120 {
            pet.tick(30.0, &bare_world(w, h, tw));
        }
        // After ~3.6 s it's still up high — it flew, it didn't drop to the floor.
        assert!(
            pet.pos().1 < h * 0.5,
            "owl stayed aloft (y={}, floor={})",
            pet.pos().1,
            h - th
        );
    }

    #[test]
    fn ascii_pet_moves_through_a_graph() {
        // A little ASCII movie: the sheep (🐑) is dropped above a mid-air ledge,
        // lands on it, walks along it, then walks off the end and drops to the
        // floor. Printed as frames so a human can watch it move; asserts it
        // actually visits the ledge and ends up on the floor. A sheep (not the
        // owl, which now flies) so it falls and lands deterministically.
        use std::fmt::Write as _;
        let lp = load_pet("blue_sheep").expect("sheep assets");
        let (w, h) = (240.0f32, 140.0f32);
        let (tw, th) = (24.0f32, 28.0f32);
        let ledge = [Surface {
            y: 84.0,
            x0: 48.0,
            x1: 168.0,
        }];
        let (cols, rows) = ((w / tw) as usize, (h / th) as usize);
        let ledge_row = (ledge[0].y / th) as usize;
        let render = |pet: &Pet| -> String {
            let (x, y) = pet.pos();
            let pc = ((tw.mul_add(0.5, x) / tw) as usize).min(cols - 1);
            let pr = ((y / th) as usize).min(rows - 1);
            (0..rows)
                .map(|r| {
                    (0..cols)
                        .map(|c| {
                            if r == pr && c == pc {
                                '🐑'
                            } else if r == rows - 1 {
                                '━' // floor
                            } else if r == ledge_row {
                                let cx = (c as f32).mul_add(tw, tw * 0.5);
                                if cx >= ledge[0].x0 && cx <= ledge[0].x1 {
                                    '━'
                                } else {
                                    '·'
                                }
                            } else {
                                '·'
                            }
                        })
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        let mut pet = Pet::new(lp.def, w, h, tw, th);
        pet.grab();
        pet.set_pos(84.0, 0.0); // released above the ledge
        pet.release();

        let mut on_ledge = false;
        let mut movie = String::new();
        for i in 0..260 {
            pet.tick(
                16.0,
                &World {
                    w,
                    h,
                    tile_w: tw,
                    tile_h: th,
                    surfaces: &ledge,
                },
            );
            let feet = pet.pos().1 + th;
            let cx = tw.mul_add(0.5, pet.pos().0);
            if (feet - ledge[0].y).abs() < 2.0 && cx >= ledge[0].x0 && cx <= ledge[0].x1 {
                on_ledge = true;
            }
            if i % 40 == 0 {
                let _ = write!(movie, "t={i}:\n{}\n\n", render(&pet));
            }
        }
        let _ = write!(movie, "final:\n{}\n", render(&pet));
        eprintln!("\n{movie}");

        assert!(on_ledge, "the sheep fell onto the mid-air ledge");
    }

    #[test]
    fn grounded_pets_keep_walking_rather_than_performing() {
        // The tuning: a grounded pet that finishes its walk should usually just
        // walk again, so specials (sit, look-around, …) become the exception.
        // Count how often the walk sequence loops back to walk when grounded vs
        // airborne over many sequence-ends; grounded must stick to walking far
        // more, and comfortably more than half the time.
        let lp = load_pet("blue_sheep").expect("sheep assets");
        let mut pet = Pet::new(lp.def, 200.0, 120.0, 24.0, 28.0);
        let start = pet.def.start;
        let walk = pet.def.anims[&start].clone();

        let count_walk = |pet: &mut Pet, grounded: bool| {
            pet.anim = start; // pick_next keys the bias off the *current* anim
            let mut kept = 0;
            for _ in 0..1000 {
                if pet.pick_next(&walk, grounded) == Some(start) {
                    kept += 1;
                }
            }
            kept
        };

        let grounded = count_walk(&mut pet, true);
        let airborne = count_walk(&mut pet, false);
        assert!(
            grounded > 700,
            "grounded pet mostly keeps walking (got {grounded}/1000)"
        );
        assert!(
            grounded > airborne,
            "the grounded walk-bias raises the keep-walking rate \
             (grounded {grounded} vs airborne {airborne} of 1000)"
        );
    }

    #[test]
    fn ascii_pet_at_every_screen_position() {
        // Drive the pet to each canonical spot — corners, walls, ceiling, floor,
        // centre — and confirm its `pos()` renders into the right ASCII cell.
        // `grab()` freezes physics so `set_pos` sticks and the frame is exactly
        // where we put it. Prints an ASCII graph per position (🐑 = pet).
        use std::fmt::Write as _;
        let lp = load_pet("owl").expect("owl assets");
        let (w, h) = (200.0f32, 120.0f32);
        let (tw, th) = (40.0f32, 40.0f32);
        let (cols, rows) = ((w / tw) as usize, (h / th) as usize); // 5 × 3
        let mut pet = Pet::new(lp.def, w, h, tw, th);
        pet.grab();

        let spots = [
            ("top-left corner", 0.0, 0.0),
            ("ceiling middle", (w - tw) / 2.0, 0.0),
            ("top-right corner", w - tw, 0.0),
            ("left wall middle", 0.0, (h - th) / 2.0),
            ("dead centre", (w - tw) / 2.0, (h - th) / 2.0),
            ("right wall middle", w - tw, (h - th) / 2.0),
            ("bottom-left corner", 0.0, h - th),
            ("floor middle", (w - tw) / 2.0, h - th),
            ("bottom-right corner", w - tw, h - th),
        ];

        let mut out = String::new();
        for (label, x, y) in spots {
            pet.set_pos(x, y);
            let (px_, py_) = pet.pos();
            assert!(
                (px_ - x).abs() < 0.01 && (py_ - y).abs() < 0.01,
                "{label}: position round-trips"
            );
            let pc = ((tw.mul_add(0.5, px_) / tw) as usize).min(cols - 1);
            let pr = ((py_ / th) as usize).min(rows - 1);
            let grid: Vec<String> = (0..rows)
                .map(|r| (0..cols).map(|c| if r == pr && c == pc { '🐑' } else { '·' }).collect())
                .collect();
            let _ = writeln!(out, "{label} ({x},{y}):\n{}\n", grid.join("\n"));
            let n = grid.iter().flat_map(|s| s.chars()).filter(|&ch| ch == '🐑').count();
            assert_eq!(n, 1, "{label}: pet renders in exactly one cell");
            let ecol = ((tw.mul_add(0.5, x) / tw) as usize).min(cols - 1);
            let erow = ((y / th) as usize).min(rows - 1);
            assert_eq!((pr, pc), (erow, ecol), "{label}: rendered at the expected cell");
        }
        eprintln!("\n{out}");
    }

    #[test]
    fn ascii_pet_flips_facing_at_the_wall() {
        // The mirror: the owl spawns facing left (◀); when it walks into the left
        // wall the XML `<action>flip>` toggles `facing`, and the app then draws
        // the mirrored sheet so it faces right (▶). Assert the flip sheet is a
        // real, distinct mirror and that `flipped()` toggles, shown with chars.
        let lp = load_pet("owl").expect("owl assets");
        assert_ne!(
            lp.png, lp.png_flip,
            "the flip sheet is a distinct mirrored image, not a copy"
        );
        assert_eq!(
            png_dims(&lp.png),
            png_dims(&lp.png_flip),
            "same geometry as the normal sheet"
        );

        let (w, h) = (200.0f32, 80.0f32);
        let (tw, th) = (40.0f32, 40.0f32);
        let (cols, rows) = ((w / tw) as usize, (h / th) as usize);
        let render = |p: &Pet| -> String {
            let (x, y) = p.pos();
            let pc = ((tw.mul_add(0.5, x) / tw) as usize).min(cols - 1);
            let pr = ((y / th) as usize).min(rows - 1);
            let glyph = if p.flipped() { '▶' } else { '◀' }; // ▶ = mirrored sheet
            (0..rows)
                .map(|r| {
                    (0..cols)
                        .map(|c| if r == pr && c == pc { glyph } else { '·' })
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        let mut pet = Pet::new(lp.def, w, h, tw, th);
        assert!(!pet.flipped(), "spawns facing left (◀ → normal sheet)");
        let spawn_art = render(&pet);

        let mut turned_art = None;
        for _ in 0..200 {
            pet.tick(
                100.0,
                &World {
                    w,
                    h,
                    tile_w: tw,
                    tile_h: th,
                    surfaces: &[],
                },
            );
            if pet.pos().0 <= 1.0 && pet.flipped() {
                turned_art = Some(render(&pet));
                break;
            }
        }
        let turned_art = turned_art.expect("pet turned at the wall to face right");
        eprintln!("spawn (facing left):\n{spawn_art}\n\nafter wall-turn (facing right):\n{turned_art}\n");
        assert!(
            spawn_art.contains('◀'),
            "spawn frame draws the left-facing (normal) sprite"
        );
        assert!(
            turned_art.contains('▶'),
            "post-turn frame draws the right-facing (mirrored) sprite"
        );
    }

    #[test]
    fn pets_never_freeze_in_a_dead_end() {
        // Regression: a sheep used to get stuck FOREVER at the top of the screen
        // in `alien_kill` (a death animation with an empty `<next>`). No pet may
        // stay frozen — same anim id AND position — for long. Walking keeps the
        // anim but moves; idle changes anim or animates in place and wakes up. The
        // pre-fix freeze was 174_304 ticks; legit idle tops out around 2_775.
        const MAX_FROZEN: u32 = 10_000; // ≈2.7 min of sim
        for id in ["owl", "blue_sheep", "blue_ham_ham"] {
            let Some(lp) = load_pet(id) else { continue };
            let (w, h) = (1200.0f32, 700.0f32);
            let (tw, th) = (lp.sheet_w / lp.def.tilesx as f32, lp.sheet_h / lp.def.tilesy as f32);
            let mut pet = Pet::new(lp.def, w, h, tw, th);
            let (mut ka, mut kx, mut ky, mut run, mut worst) = (u32::MAX, 0.0f32, 0.0f32, 0u32, 0u32);
            for _ in 0..150_000 {
                pet.tick(
                    16.0,
                    &World {
                        w,
                        h,
                        tile_w: tw,
                        tile_h: th,
                        surfaces: &[],
                    },
                );
                let (x, y) = pet.pos();
                if pet.anim == ka && (x - kx).abs() < 1.0 && (y - ky).abs() < 1.0 {
                    run += 1;
                } else {
                    (ka, kx, ky, run) = (pet.anim, x, y, 1);
                }
                worst = worst.max(run);
            }
            assert!(
                worst < MAX_FROZEN,
                "{id}: frozen for {worst} ticks — stuck in a dead-end animation"
            );
        }
    }

    #[test]
    fn dead_end_animation_respawns_the_pet() {
        // Upstream (`FormPet.SetNewAnimation`: `if (id < 0) ... spawn!`) respawns
        // the pet when an animation dead-ends. Shove the sheep into `alien_kill`
        // (id 62 — empty `<next>`) at the top of the screen and confirm it
        // respawns at the bottom instead of freezing there.
        let lp = load_pet("blue_sheep").expect("sheep assets");
        let (w, h) = (1000.0f32, 700.0f32);
        let (tw, th) = (lp.sheet_w / lp.def.tilesx as f32, lp.sheet_h / lp.def.tilesy as f32);
        let mut pet = Pet::new(lp.def, w, h, tw, th);
        pet.set_pos(400.0, 0.0); // top of the screen
        pet.anim = 62; // alien_kill — a dead end
        pet.frame_i = 0;
        let spawn_y = h - th;
        let mut respawned = false;
        for _ in 0..300 {
            pet.tick(
                60.0,
                &World {
                    w,
                    h,
                    tile_w: tw,
                    tile_h: th,
                    surfaces: &[],
                },
            );
            if pet.anim != 62 && (pet.pos().1 - spawn_y).abs() < 2.0 {
                respawned = true;
                break;
            }
        }
        assert!(respawned, "the dead-end animation respawned the pet at the bottom");
    }

    #[test]
    fn thrown_pet_recovers_to_walking() {
        // Regression: throwing a pet up must not leave it stuck in a fall pose
        // (the sheep's `fall_face`) on the ground. After it settles it should be
        // back in its start (walk) animation, on the floor.
        let lp = load_pet("blue_sheep").expect("sheep assets");
        let (w, h) = (1000.0f32, 700.0f32);
        let (tw, th) = (lp.sheet_w / lp.def.tilesx as f32, lp.sheet_h / lp.def.tilesy as f32);
        let start = lp.def.start;
        let mut pet = Pet::new(lp.def, w, h, tw, th);
        pet.grab();
        pet.set_pos(w * 0.5, 40.0);
        pet.release();
        let mut saw_start_after_settling = false;
        for i in 0..900 {
            pet.tick(
                30.0,
                &World {
                    w,
                    h,
                    tile_w: tw,
                    tile_h: th,
                    surfaces: &[],
                },
            );
            if i >= 700 && pet.anim == start {
                saw_start_after_settling = true;
            }
        }
        let floor = h - th;
        assert!(
            (pet.pos().1 - floor).abs() < 2.0,
            "settled on the floor (y={})",
            pet.pos().1
        );
        assert!(
            saw_start_after_settling,
            "recovered to the walk animation (not stuck face-down)"
        );
    }

    #[test]
    fn drag_freezes_physics_then_releases() {
        let def = PetDef::parse(XML).unwrap();
        let mut pet = Pet::new(def, 400.0, 300.0, 49.0, 49.0);
        pet.grab();
        assert!(pet.is_dragging());
        pet.set_pos(123.0, 45.0);
        let before = pet.pos();
        pet.tick(1000.0, &bare_world(400.0, 300.0, 49.0));
        assert_eq!(pet.pos(), before, "frozen while dragged");
        pet.release();
        assert!(!pet.is_dragging());
    }

    #[test]
    fn grabbing_plays_the_astonished_animation() {
        // Held ⇒ astonished. The sheep's `scream` (id 14) is its `drag`/surprised
        // pose; grabbing swaps to it.
        let lp = load_pet("blue_sheep").expect("sheep assets");
        let mut pet = Pet::new(lp.def, 800.0, 600.0, 40.0, 40.0);
        pet.grab();
        assert!(pet.is_dragging());
        assert_eq!(pet.anim, 14, "grabbed pet shows the scream/astonished animation");
    }

    #[test]
    fn dropped_pet_falls_instead_of_walking_a_wall() {
        // Grab the sheep mid-air while it's in a wall-climbing animation, drop it,
        // and confirm it falls to the floor instead of resuming the climb in place.
        let lp = load_pet("blue_sheep").expect("sheep assets");
        let (w, h) = (1000.0f32, 700.0f32);
        let (tw, th) = (lp.sheet_w / lp.def.tilesx as f32, lp.sheet_h / lp.def.tilesy as f32);
        let mut pet = Pet::new(lp.def, w, h, tw, th);
        pet.grab();
        pet.set_pos(500.0, 100.0); // mid-air
        pet.anim = 37; // walk_up — a wall climb with no gravity of its own
        pet.release();
        assert_ne!(pet.anim, 37, "release leaves the wall-climbing animation");
        for _ in 0..600 {
            pet.tick(
                16.0,
                &World {
                    w,
                    h,
                    tile_w: tw,
                    tile_h: th,
                    surfaces: &[],
                },
            );
        }
        assert!(
            (pet.pos().1 + th - h).abs() < 2.0,
            "dropped pet fell to the floor (y={})",
            pet.pos().1
        );
    }

    #[test]
    fn sheep_grazes_owl_does_not() {
        let sheep = load_pet("blue_sheep").expect("sheep");
        assert!(
            Pet::new(sheep.def, 800.0, 600.0, 40.0, 40.0).can_graze(),
            "sheep grazes"
        );
        let owl = load_pet("owl").expect("owl");
        assert!(
            !Pet::new(owl.def, 800.0, 600.0, 49.0, 49.0).can_graze(),
            "owl doesn't graze"
        );
    }

    #[test]
    fn grazing_holds_the_pet_in_place() {
        let lp = load_pet("blue_sheep").expect("sheep");
        let (tw, th) = (lp.sheet_w / lp.def.tilesx as f32, lp.sheet_h / lp.def.tilesy as f32);
        let mut pet = Pet::new(lp.def, 800.0, 600.0, tw, th);
        pet.set_pos(300.0, 200.0);
        pet.start_grazing();
        assert!(pet.is_grazing() && !pet.is_walking());
        let before = pet.pos();
        for _ in 0..80 {
            pet.tick(30.0, &bare_world(800.0, 600.0, tw));
        }
        assert_eq!(pet.pos(), before, "grazes in place — no movement or transitions");
        assert!(pet.is_grazing(), "keeps grazing until told to stop");
        pet.stop_grazing();
        assert!(!pet.is_grazing() && pet.is_walking());
    }

    #[test]
    fn summon_grows_the_herd_dismiss_clears_it() {
        let mut ov = PetOverlay::default();
        assert!(!ov.is_active() && ov.count() == 0);
        ov.summon(1000.0, 700.0);
        ov.summon(1000.0, 700.0);
        ov.summon(1000.0, 700.0);
        assert_eq!(ov.count(), 3, "each summon adds one (no cap)");
        assert!(ov.is_active());
        ov.dismiss_all();
        assert_eq!(ov.count(), 0);
        assert!(ov.grass.is_empty(), "dismiss-all clears the grass too");
    }

    #[test]
    fn grass_grows_on_the_tab_ledges() {
        let mut ov = PetOverlay::default();
        ov.ledges.borrow_mut().push(Surface {
            y: 120.0,
            x0: 0.0,
            x1: 500.0,
        });
        // ~4.8 s of sim: past the grow interval, so at least one tuft sprouts.
        for _ in 0..300 {
            ov.advance(16.0, 1000.0, 700.0);
        }
        assert!(!ov.grass.is_empty(), "grass sprouted on the ledge");
        assert!(
            ov.grass
                .iter()
                .all(|g| (g.y - 120.0).abs() < 0.1 && (0.0..=500.0).contains(&g.x)),
            "every tuft sits on the ledge line"
        );
    }

    #[test]
    fn auto_spawn_never_exceeds_the_asked_count() {
        // The grazing herd may top up toward the summoned count but must never
        // overflow it — even with grass sitting around for a long time.
        let mut ov = PetOverlay::default();
        let (w, h) = (1200.0f32, 700.0f32);
        ov.ledges.borrow_mut().push(Surface {
            y: 120.0,
            x0: 0.0,
            x1: 900.0,
        });
        ov.summon(w, h);
        ov.summon(w, h);
        assert_eq!(ov.count(), 2, "asked for two");
        // ~40 s of sim with grass present — many spawn intervals would have fired.
        for _ in 0..2500 {
            ov.advance(16.0, w, h);
        }
        assert_eq!(ov.count(), 2, "auto-spawn stayed within the asked count");
    }

    #[test]
    fn a_sheep_eats_a_grass_tuft() {
        let mut ov = PetOverlay::default();
        let (w, h) = (1000.0f32, 700.0f32);
        ov.ledges.borrow_mut().push(Surface {
            y: 120.0,
            x0: 0.0,
            x1: 500.0,
        });
        let mut lp = PetOverlay::build("blue_sheep", w, h).expect("sheep");
        let (tw, th) = lp.tile_wh;
        lp.pet.set_pos(tw.mul_add(-0.5, 200.0), 120.0 - th); // feet at (200, 120), on the ledge
        ov.pets.push(lp);
        ov.grass.push(Grass {
            x: 200.0,
            y: 120.0,
            amount: 1.0,
        }); // right under its mouth

        let mut grazed = false;
        for _ in 0..220 {
            // ~3.5 s — long enough to eat one tuft, short of a second sprout
            ov.advance(16.0, w, h);
            if ov.pets[0].pet.is_grazing() {
                grazed = true;
            }
            if ov.grass.is_empty() {
                break;
            }
        }
        assert!(grazed, "the sheep stopped to graze");
        assert!(
            ov.grass.iter().all(|g| (g.x - 200.0).abs() > 1.0),
            "the tuft it was eating is gone"
        );
    }
}
