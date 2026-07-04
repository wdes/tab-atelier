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
//! to the active tile, and can [`Pet::grab`]/[`Pet::drop`] it for drag-and-drop.
//!
//! Unknown animation ids are tolerated (they fall back to the start animation),
//! so a pet referencing an effect we don't model simply keeps walking.

#![cfg(feature = "pets")]

use std::collections::HashMap;
use std::path::PathBuf;

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
    anims: HashMap<u32, Anim>,
    /// Animation to start in (the `walk` id, or the lowest id as a fallback).
    start: u32,
    /// Whether any animation has a `<gravity>` fall. Pets without one (the owl)
    /// get a synthetic downward drift when airborne so a dropped pet doesn't
    /// hover; pets with one are left to their own aerial animations.
    has_gravity: bool,
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
        let mut anims: HashMap<u32, Anim> = HashMap::new();
        let mut start = u32::MAX;
        let mut walk_id = None;
        for a in anims_node.children().filter(|n| n.has_tag_name("animation")) {
            let Some(id) = a.attribute("id").and_then(|s| s.parse::<u32>().ok()) else {
                continue;
            };
            if child(a, "name").as_deref() == Some("walk") {
                walk_id = Some(id);
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
                Anim {
                    frames,
                    interval_ms: interval_ms.max(1.0),
                    start: start_v,
                    end: end_v,
                    flip,
                    next,
                    border_next,
                    gravity_next,
                },
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
            anims,
            has_gravity,
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
        }
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
            if let Some(g) = cur.gravity_next {
                // The pet has its own fall animation — play it.
                self.anim = g;
                self.frame_i = 0;
                return;
            }
            if !self.def.has_gravity {
                // A pet with no fall animation at all (the owl). Drift straight
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
            if let Some(next) = self.pick_next(cur) {
                self.anim = next;
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

    fn pick_next(&mut self, cur: &Anim) -> Option<u32> {
        for &(prob, id) in &cur.next {
            if self.rand_100() < prob {
                return Some(id);
            }
        }
        cur.next.first().map(|&(_, id)| id)
    }

    /// Begin a drag: freeze physics until [`Pet::drop`].
    pub const fn grab(&mut self) {
        self.dragging = true;
    }

    /// Move the pet to a top-left position (used while dragging).
    pub const fn set_pos(&mut self, x: f32, y: f32) {
        self.x = x;
        self.y = y;
    }

    /// Release a drag: gravity takes over on the next tick.
    pub const fn drop(&mut self) {
        self.dragging = false;
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
    let def = PetDef::parse(&xml)?;
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
    fn dropped_pet_lands_on_the_floor() {
        // Regression: a fall must resolve to the floor (not loop the fall
        // sequence "stuck upright"), and a pet with no fall animation (the owl)
        // must still drop instead of hovering. Both must settle on the floor.
        for id in ["owl", "blue_sheep"] {
            let lp = load_pet(id).unwrap_or_else(|| panic!("{id} assets"));
            let (w, h) = (1000.0f32, 700.0f32);
            let (tw, th) = (lp.sheet_w / lp.def.tilesx as f32, lp.sheet_h / lp.def.tilesy as f32);
            let floor = h - th;
            let mut pet = Pet::new(lp.def, w, h, tw, th);
            pet.grab();
            pet.set_pos(w * 0.5, 30.0);
            pet.drop();
            for _ in 0..800 {
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
            }
            assert!(
                (pet.pos().1 - floor).abs() < 2.0,
                "{id} settled on the floor (y={})",
                pet.pos().1
            );
        }
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
        pet.drop();
        assert!(!pet.is_dragging());
    }
}
