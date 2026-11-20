//! The frame task owns popup lifetimes and attribution. Rendering reads only copied data.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use eldenring::cs::{CSCam, CSCamera, CSMenuManImp, FieldInsHandle, WorldChrMan};
use fromsoftware_shared::FromStatic;
use hudhook::imgui::{Context, FontId, FontSource, TextureId, Ui};
use shared::memscan;

use super::effects::EffectTick;
use super::probe::{Event, position};
use crate::{settings, target, text};

/// Sizes as multiples of the `FontSize` setting: a critical hit or a burst, a DoT tick, the hit counter.
const CRIT_SIZE: f32 = 2.0;
const DOT_SIZE: f32 = 0.5;
const COMBO_SIZE: f32 = 0.4;
/// A number falls this far (1080p pixels) over its life while it shrinks by this fraction.
const FALL_PX: f32 = 85.0;
const SHRINK: f32 = 0.6;
/// A critical hit or a burst stays this many times longer than the `Duration` setting.
const CRIT_LIFE: f32 = 2.0;
/// Seconds between the numbers of a split hit, and how far (1080p pixels) they fan out.
const SPLIT_STAGGER: f32 = 0.07;
const FAN_PX: f32 = 60.0;
/// How much of the bar texture's shading the digits take (0 flat, 1 as dark as its darkest).
const TEXTURE_STRENGTH: f32 = 0.5;
/// A number that takes another hit rises back to its start in `REWIND` seconds, and swells and
/// brightens for `POP` seconds.
const REWIND: f32 = 0.2;
const POP: f32 = 0.25;
const POP_SCALE: f32 = 0.35;
/// The font is rasterized once at this size, so the numbers stay sharp up to it.
const FONT_PX: f32 = 96.0;
/// A number is full size at this distance from the camera, smaller farther and larger nearer.
const FULL_SIZE_METERS: f32 = 15.0;
/// Hits on one target this close together grow one number (`DamageNumbers.Combo`).
const MERGE_WINDOW: Duration = Duration::from_millis(1000);
const STALE: Duration = Duration::from_millis(500);
const MAX_POPUPS: usize = 96;
const _: () = assert!(std::mem::size_of::<FieldInsHandle>() == 8);

/// Where the digits of the font start and end under the top of the line, as a fraction of its size.
static DIGIT_TOP: AtomicU32 = AtomicU32::new(0.17f32.to_bits());
static DIGIT_BOTTOM: AtomicU32 = AtomicU32::new(0.92f32.to_bits());
static LATEST: Mutex<Option<Snapshot>> = Mutex::new(None);

/// The order is that of `DamageColors::all`, which the configured colors are indexed by.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Physical,
    Magic,
    Fire,
    Lightning,
    Holy,
    Bleed,
    Frost,
    Poison,
    Rot,
    Burn,
}

impl Kind {
    fn color(self) -> [f32; 3] {
        settings::damage_color(self as usize)
    }
}

/// The damage after the reduction per type, those above zero. Physical is what the total keeps beyond
/// the elements: it is off by a few points from the truncation of each value.
fn parts(total: i32, elements: Option<[i32; 4]>) -> Option<Vec<(Kind, i32)>> {
    let [fire, magic, lightning, holy] = elements?;
    let physical = total - fire - magic - lightning - holy;
    let all = [
        (Kind::Physical, physical),
        (Kind::Magic, magic),
        (Kind::Fire, fire),
        (Kind::Lightning, lightning),
        (Kind::Holy, holy),
    ];
    let shown: Vec<_> = all.into_iter().filter(|&(_, value)| value > 0).collect();
    (!shown.is_empty()).then_some(shown)
}

/// The color at `height` (0 at the bottom, 1 at the top) of a number made of `layers`, bottom first: each
/// layer keeps its color over its share of the height, with a short blend across the borders.
fn layer_color(layers: &[([f32; 3], f32)], height: f32) -> [f32; 3] {
    const BLEND: f32 = 0.3;
    let mut edge = 0.0;
    for pair in layers.windows(2) {
        edge += pair[0].1;
        if (height - edge).abs() < BLEND / 2.0 {
            let t = (height - (edge - BLEND / 2.0)) / BLEND;
            return std::array::from_fn(|i| pair[0].0[i] + (pair[1].0[i] - pair[0].0[i]) * t);
        }
    }
    let mut top = 0.0;
    for (color, share) in layers {
        top += share;
        if height <= top {
            return *color;
        }
    }
    layers.last().map_or([1.0; 3], |layer| layer.0)
}

fn element(components: Option<[f32; 5]>) -> Kind {
    let Some(values) = components else { return Kind::Physical };
    let mut largest = 0.0;
    let mut kind = Kind::Physical;
    for (value, candidate) in
        values[1..].iter().zip([Kind::Magic, Kind::Fire, Kind::Lightning, Kind::Holy])
    {
        if value.is_finite() && *value > largest {
            largest = *value;
            kind = candidate;
        }
    }
    kind
}

#[derive(Clone)]
struct Popup {
    handle: u64,
    anchor: [f32; 3],
    label: String,
    kind: Kind,
    color: [f32; 3],
    /// The color and share of each type of a combined number, bottom first; empty for a flat color.
    layers: Vec<([f32; 3], f32)>,
    /// How many hits the number adds up; the counter shows from 2.
    hits: u32,
    /// Whether a later hit was added, and how far through its life the number was then.
    merged: bool,
    from: f32,
    /// Where a split number flies, from the left (below 0) to the right.
    fan: f32,
    /// Index in a split hit: later numbers leave later.
    row: usize,
    critical: bool,
    /// A small number: the tick of an effect, not a hit or a burst.
    dot: bool,
    sequence: usize,
    at: Instant,
}

impl Popup {
    /// How long after its hit a number of a split hit waits, so the numbers leave one by one.
    fn delay(&self) -> f32 {
        self.row as f32 * SPLIT_STAGGER
    }

    fn burst(&self) -> bool {
        matches!(self.kind, Kind::Bleed | Kind::Frost)
    }

    fn life(&self) -> f32 {
        settings::damage_duration() * if self.critical || self.burst() { CRIT_LIFE } else { 1.0 }
    }

    /// How far through its life the number is (below 0 before it starts), and how strong the swell
    /// of a merged hit still is (1 to 0).
    fn progress(&self) -> (f32, f32) {
        let since = self.at.elapsed().as_secs_f32() - self.delay();
        if since < 0.0 {
            return (-1.0, 0.0);
        }
        if !self.merged {
            return (since / self.life(), 0.0);
        }
        let t = if since < REWIND {
            let left = 1.0 - since / REWIND;
            self.from * left * left
        } else {
            (since - REWIND) / self.life()
        };
        (t, (1.0 - since / POP).clamp(0.0, 1.0))
    }

    fn lasts(&self) -> f32 {
        self.delay() + self.life() + if self.merged { REWIND } else { 0.0 }
    }
}

#[derive(Clone)]
struct Combo {
    handle: u64,
    sequence: usize,
    total: i32,
    parts: Vec<(Kind, i32)>,
    critical: bool,
    last: Instant,
    hits: u32,
    merged: bool,
    from: f32,
}

#[derive(Clone, Copy)]
struct Camera {
    rows: [[f32; 4]; 4],
    fov: f32,
    aspect: f32,
    near: f32,
}

impl Camera {
    fn read() -> Option<Self> {
        // SAFETY: singleton access fails while the game is not initialized.
        let camera = unsafe { CSCamera::instance() }.ok()?;
        let base = std::ptr::from_ref(camera) as usize;
        let ptr = memscan::read_safe::<usize>(base + std::mem::offset_of!(CSCamera, pers_cam_1))
            .filter(|&p| p >= 0x10000)?;
        let rows = memscan::read_safe::<[[f32; 4]; 4]>(ptr + std::mem::offset_of!(CSCam, matrix))?;
        let [fov, aspect, near] =
            memscan::read_safe::<[f32; 3]>(ptr + std::mem::offset_of!(CSCam, fov))?;
        (rows.iter().flatten().all(|n| n.is_finite())
            && fov.is_finite()
            && fov > 0.01
            && fov < 3.13
            && aspect.is_finite()
            && aspect > 0.1
            && near.is_finite()
            && near >= 0.0)
            .then_some(Self { rows, fov, aspect, near })
    }

    /// The screen position of `anchor` and its distance along the view axis, in meters.
    fn project(&self, anchor: [f32; 3], [x, y, w, h]: [f32; 4]) -> Option<([f32; 2], f32)> {
        let delta = std::array::from_fn::<_, 3, _>(|i| anchor[i] - self.rows[3][i]);
        let dot = |row: usize| (0..3).map(|i| delta[i] * self.rows[row][i]).sum::<f32>();
        let depth = dot(2);
        if depth <= self.near.max(0.1) {
            return None;
        }
        let half_height = depth * (self.fov * 0.5).tan();
        let nx = dot(0) / (half_height * self.aspect);
        let ny = dot(1) / half_height;
        (nx.is_finite() && ny.is_finite() && nx.abs() < 1.1 && ny.abs() < 1.1)
            .then_some(([x + (nx + 1.0) * 0.5 * w, y + (1.0 - ny) * 0.5 * h], depth))
    }
}

#[derive(Clone)]
struct Snapshot {
    camera: Camera,
    popups: Vec<Popup>,
    at: Instant,
}

#[derive(Default)]
pub(super) struct Tracker {
    popups: Vec<Popup>,
    combos: Vec<Combo>,
    effects: usize,
    last: Option<Instant>,
    player: usize,
}

impl Tracker {
    pub fn update(&mut self, events: &[Event], ticks: &[EffectTick]) {
        let now = Instant::now();
        let player = shared::player::main_player_chr_ins_ptr().map_or(0, |p| p as usize);
        // SAFETY: singleton access fails while the menu manager is absent.
        let cutscene = unsafe { CSMenuManImp::instance() }.is_ok_and(|menu| {
            let base = std::ptr::from_ref(menu) as usize;
            memscan::read_safe::<[u8; 2]>(base + 0x18) == Some([0, 0])
        });
        let camera = Camera::read();
        let active = settings::damage_numbers()
            && player != 0
            && !cutscene
            && crate::radar::in_control()
            && camera.is_some();
        let changed =
            self.player != player || self.last.is_some_and(|at| now.duration_since(at) > STALE);
        self.last = Some(now);
        self.player = player;
        if !active || changed {
            self.popups.clear();
            self.combos.clear();
        }
        if active && !changed {
            self.popups.retain(|p| p.at.elapsed().as_secs_f32() < p.lasts());
            for event in events {
                self.hit(event);
            }
            let player_handle = memscan::read_safe::<u64>(player.wrapping_add(8));
            for tick in ticks {
                self.effect(tick, player_handle);
            }
        }
        if let Ok(mut guard) = LATEST.lock() {
            *guard = camera.filter(|_| active).map(|camera| Snapshot {
                camera,
                popups: self.popups.clone(),
                at: now,
            });
        }
    }

    fn push(&mut self, popup: Popup) {
        if settings::damage_probe() {
            shared::logger::log(&format!(
                "Damage popup: hit {} target {} damage {} kind {:?} critical {} anchor {:?}",
                popup.sequence.wrapping_add(1),
                popup.handle,
                popup.label,
                popup.kind,
                popup.critical,
                popup.anchor,
            ));
        }
        if popup.row == 0
            && !popup.dot
            && self
                .popups
                .iter()
                .filter(|p| p.handle == popup.handle && p.row == 0 && !p.dot)
                .count()
                >= 5
            && let Some(oldest) =
                self.popups.iter().find(|p| p.handle == popup.handle && !p.dot).map(|p| p.sequence)
        {
            self.popups.retain(|p| p.handle != popup.handle || p.sequence != oldest);
        }
        if self.popups.len() == MAX_POPUPS {
            self.popups.remove(0);
        }
        self.popups.push(popup);
    }

    fn hit(&mut self, event: &Event) {
        self.hit_with(event, settings::damage_combo());
    }

    /// Adds the damage to the number the target still has from a hit just before, when `merge`.
    fn combine(&mut self, hit: Combo, merge: bool) -> Combo {
        let found = merge
            .then(|| {
                self.combos
                    .iter()
                    .position(|c| c.handle == hit.handle && c.last.elapsed() < MERGE_WINDOW)
            })
            .flatten();
        let Some(index) = found else {
            if merge {
                self.combos.retain(|c| c.handle != hit.handle);
                self.combos.push(hit.clone());
            }
            return hit;
        };
        let from = self
            .popups
            .iter()
            .filter(|p| p.handle == hit.handle && p.sequence == self.combos[index].sequence)
            .map(|p| p.progress().0.clamp(0.0, 0.9))
            .fold(0.0, f32::max);
        let combo = &mut self.combos[index];
        combo.merged = true;
        combo.hits += 1;
        combo.from = from;
        combo.total += hit.total;
        combo.critical |= hit.critical;
        combo.last = hit.last;
        for (kind, value) in hit.parts {
            match combo.parts.iter_mut().find(|p| p.0 == kind) {
                Some(part) => part.1 += value,
                None => combo.parts.push((kind, value)),
            }
        }
        combo.parts.sort_by_key(|p| p.0 as u8);
        let combo = combo.clone();
        // The new total replaces the numbers of the combo; a burst of the same hits stays.
        self.popups.retain(|p| {
            p.handle != combo.handle
                || p.sequence != combo.sequence
                || matches!(p.kind, Kind::Bleed | Kind::Frost)
        });
        combo
    }

    /// Puts the numbers of `combo` on screen.
    fn show(&mut self, combo: &Combo, anchor: [f32; 3]) {
        let kind = combo.parts.iter().max_by_key(|p| p.1).map_or(Kind::Physical, |p| p.0);
        let popup = Popup {
            handle: combo.handle,
            anchor,
            label: combo.total.to_string(),
            kind,
            color: kind.color(),
            layers: Vec::new(),
            hits: combo.hits,
            merged: combo.merged,
            from: combo.from,
            fan: 0.0,
            row: 0,
            critical: combo.critical,
            dot: false,
            sequence: combo.sequence,
            at: combo.last,
        };
        match settings::damage_style() {
            settings::DamageStyle::Split if !combo.critical => {
                let count = combo.parts.len();
                for (row, &(kind, value)) in combo.parts.iter().enumerate() {
                    self.push(Popup {
                        label: value.to_string(),
                        kind,
                        color: kind.color(),
                        row,
                        fan: row as f32 - (count - 1) as f32 / 2.0,
                        critical: popup.critical && row == 0,
                        hits: if row == 0 { popup.hits } else { 1 },
                        ..popup.clone()
                    });
                }
            }
            _ => {
                let sum = combo.parts.iter().map(|p| p.1).sum::<i32>().max(1) as f32;
                let layers = if combo.parts.len() < 2 {
                    Vec::new()
                } else {
                    combo.parts.iter().map(|&(k, v)| (k.color(), v as f32 / sum)).collect()
                };
                self.push(Popup { layers, ..popup.clone() });
            }
        }
    }

    fn hit_with(&mut self, event: &Event, merge: bool) {
        let Some(handle) = event.target else { return };
        if !event.own {
            return;
        }
        let (Some(damage), Some(mut anchor)) = (event.damage.filter(|&n| n > 0), event.position)
        else {
            return;
        };
        if event.at.elapsed() > STALE {
            return;
        }
        anchor[1] += 1.5;
        let hit_parts = parts(damage, event.elements)
            .unwrap_or_else(|| vec![(element(event.components), damage)]);
        let combo = self.combine(
            Combo {
                handle,
                sequence: event.sequence,
                total: damage,
                parts: hit_parts,
                critical: event.hit_kind == Some(2),
                last: event.at,
                hits: 1,
                merged: false,
                from: 0.0,
            },
            merge,
        );
        self.show(&combo, anchor);
    }

    /// A tick of an effect on an enemy, each one its own number: poison and scarlet rot of yours as small
    /// ones, the bursts of bleed and frost as big ones. The effects that carry no kind and are credited to
    /// the player are burns (Black Flame and the like). What the game credits to someone else or to nobody
    /// (and the procs of death blight and madness) shows small in the color of physical damage, on hostile
    /// enemies only.
    fn effect(&mut self, tick: &EffectTick, player: Option<u64>) {
        let Some(handle) = tick.handle else { return };
        if !settings::damage_effects()
            || tick.changes[0] >= 0
            || Some(handle) == player
            || tick.at.elapsed() > STALE
        {
            return;
        }
        let known = match tick.kind {
            20 => Some(Kind::Poison),
            21 => Some(Kind::Rot),
            22 => Some(Kind::Bleed),
            23 => Some(Kind::Frost),
            -2 => Some(Kind::Burn),
            _ => None,
        };
        let (kind, hostile) = match known.filter(|_| player == Some(tick.causer as u64)) {
            Some(kind) => (kind, false),
            None => (Kind::Physical, true),
        };
        let burst = matches!(kind, Kind::Bleed | Kind::Frost);
        let Some(mut anchor) = enemy_position(handle, hostile) else { return };
        anchor[1] += if burst { 1.5 } else { 1.0 };
        self.show_effect(handle, kind, tick.changes[0].unsigned_abs(), anchor);
    }

    fn show_effect(&mut self, handle: u64, kind: Kind, loss: u32, anchor: [f32; 3]) {
        self.effects += 1;
        self.push(Popup {
            handle,
            anchor,
            label: loss.to_string(),
            kind,
            color: kind.color(),
            layers: Vec::new(),
            hits: 1,
            merged: false,
            from: 0.0,
            fan: 0.0,
            row: 0,
            critical: false,
            dot: !matches!(kind, Kind::Bleed | Kind::Frost),
            // Apart from the sequences of the hits, which count up from 0.
            sequence: usize::MAX - self.effects,
            at: Instant::now(),
        });
    }
}

/// Where the character with this handle stands, `None` when it is gone or, with `hostile`, is not a hostile enemy.
fn enemy_position(handle: u64, hostile: bool) -> Option<[f32; 3]> {
    // SAFETY: singleton access fails while the world is unavailable.
    let world = unsafe { WorldChrMan::instance() }.ok()?;
    let raw = handle.to_ne_bytes();
    let handle = memscan::read_safe::<FieldInsHandle>(raw.as_ptr() as usize)?;
    let chr = target::chr_by_handle(world, handle)?;
    if hostile && !crate::radar::enemies::is_hostile(chr) {
        return None;
    }
    position(std::ptr::from_ref(chr) as usize)
}

/// The font handle is a raw pointer; it is made and used on the render thread only.
#[derive(Clone, Copy)]
pub struct NumberFont(FontId);

// SAFETY: the loop that owns the handle is moved to the render thread before any frame runs.
unsafe impl Send for NumberFont {}
// SAFETY: as above.
unsafe impl Sync for NumberFont {}

/// Impact, the font the numbers read best in; it ships with Windows, not with the game.
pub fn load_font(ctx: &mut Context) -> Option<NumberFont> {
    let root = std::env::var_os("SystemRoot")?;
    let data = std::fs::read(std::path::Path::new(&root).join("Fonts").join("impact.ttf")).ok()?;
    let fonts = ctx.fonts();
    fonts.add_font(&[FontSource::DefaultFontData { config: None }]);
    let font = FontSource::TtfData { data: &data, size_pixels: FONT_PX, config: None };
    let id = fonts.add_font(&[font]);
    if let Err(err) = texture_digits(ctx, id) {
        shared::logger::warn(&format!("Damage numbers: the bar texture was not applied ({err})"));
    }
    Some(NumberFont(id))
}

/// Multiplies the digits of the font by the grain and light of the game's `Bar.png`, so the numbers
/// are not a flat, glossy fill. The font atlas is built here and uploaded by the overlay after.
fn texture_digits(ctx: &mut Context, id: FontId) -> Result<(), String> {
    let (bar, bar_width, _) = crate::textures::bar_pixels()?;
    let (bar_width, atlas) = (bar_width as usize, ctx.fonts());
    let texture = atlas.build_rgba32_texture();
    let (width, height) = (texture.width as usize, texture.height as usize);
    // SAFETY: the atlas owns its RGBA pixels (width x height x 4 bytes), and nothing else
    // reads them before the overlay uploads them.
    let pixels = unsafe {
        let raw = hudhook::imgui::internal::RawCast::raw(&*atlas);
        let ptr = raw.TexPixelsRGBA32 as *mut u8;
        if ptr.is_null() {
            return Err("no pixels".into());
        }
        std::slice::from_raw_parts_mut(ptr, width * height * 4)
    };
    let font = atlas.get_font(id).ok_or("font missing")?;
    for (n, digit) in ('0'..='9').enumerate() {
        // SAFETY: `Font` is the layout of ImFont, and the atlas is built, so its glyphs exist.
        let glyph = unsafe {
            hudhook::imgui::sys::ImFont_FindGlyph(
                std::ptr::from_ref(font).cast_mut().cast(),
                digit as hudhook::imgui::sys::ImWchar,
            )
            .as_ref()
        };
        let Some(glyph) = glyph else { continue };
        if digit == '0' {
            DIGIT_TOP.store((glyph.Y0 / FONT_PX).to_bits(), Ordering::Relaxed);
            DIGIT_BOTTOM.store((glyph.Y1 / FONT_PX).to_bits(), Ordering::Relaxed);
        }
        let (x0, x1) =
            ((glyph.U0 * width as f32) as usize, (glyph.U1 * width as f32).ceil() as usize);
        let (y0, y1) =
            ((glyph.V0 * height as f32) as usize, (glyph.V1 * height as f32).ceil() as usize);
        for y in y0..y1.min(height) {
            // The visible rows of the bar texture, 2 to 23, stretched over the glyph.
            let row = 2 + (y - y0) * 21 / (y1 - y0).max(1);
            for x in x0..x1.min(width) {
                let light =
                    bar[(row * bar_width + (x - x0 + n * 37) % bar_width) * 4] as f32 / 255.0;
                let factor = 1.0 - TEXTURE_STRENGTH * (1.0 - light);
                let pixel = &mut pixels[(y * width + x) * 4..][..3];
                for channel in pixel {
                    *channel = (*channel as f32 * factor) as u8;
                }
            }
        }
    }
    Ok(())
}

pub fn draw(
    ui: &Ui,
    area: [f32; 4],
    alpha: f32,
    font: Option<NumberFont>,
    mark: Option<TextureId>,
) {
    if !settings::damage_numbers() {
        return;
    }
    let _font = font.map(|NumberFont(id)| ui.push_font(id));
    let snapshot = LATEST.lock().ok().and_then(|guard| guard.clone());
    let Some(snapshot) = snapshot.filter(|s| s.at.elapsed() < STALE) else { return };
    let scale = area[3] / 1080.0 * settings::damage_scale();
    let [width, height] = ui.io().display_size;
    let viewport = if width / height > snapshot.camera.aspect {
        let w = height * snapshot.camera.aspect;
        [(width - w) * 0.5, 0.0, w, height]
    } else {
        let h = width / snapshot.camera.aspect;
        [0.0, (height - h) * 0.5, width, h]
    };
    for popup in snapshot.popups {
        let (t, pulse) = popup.progress();
        if !(0.0..1.0).contains(&t) {
            continue;
        }
        let Some((mut pos, depth)) = snapshot.camera.project(popup.anchor, viewport) else {
            continue;
        };
        // A burst of buildup stays readable from afar: it only grows when the target is near.
        let near = (FULL_SIZE_METERS / depth).clamp(if popup.burst() { 1.0 } else { 0.75 }, 1.5);
        let scale = scale * near;
        let side = if popup.fan == 0.0 {
            (popup.sequence.wrapping_add(usize::from(popup.burst())) % 5) as f32 - 2.0
        } else {
            0.0
        };
        pos[0] += side * (30.0 + 45.0 * t) * scale;
        pos[0] += popup.fan * FAN_PX * (0.6 + 0.9 * t) * scale;
        pos[1] += FALL_PX * t * t * scale;
        let unit =
            settings::damage_font_size() * scale * (1.0 - SHRINK * t) * (1.0 + POP_SCALE * pulse);
        let em = unit
            * if popup.critical || popup.burst() {
                CRIT_SIZE
            } else if popup.dot {
                DOT_SIZE
            } else {
                1.0
            };
        let marked = popup.critical || popup.burst();
        let label = popup.label;
        let size = text::measure(ui, &label, em);
        pos[0] -= size[0] * 0.5;
        pos[1] -= size[1] * 0.5;
        let a = alpha * (1.0 - t);
        let flash = |c: f32| c + (1.0 - c) * 0.6 * pulse;
        let shadow = [0.0, 0.0, 0.0, a];
        // The color at a height of the number, 0 at its bottom: one color, or a color per type.
        let color_at = |height: f32| {
            let [r, g, b] = if popup.layers.is_empty() {
                popup.color
            } else {
                layer_color(&popup.layers, height)
            }
            .map(flash);
            [r, g, b, a]
        };
        let paint = |text: &str, at: [f32; 2], size: f32| {
            if popup.layers.is_empty() {
                text::add_shadowed(text, at, size, color_at(0.0), shadow, 2.0 * scale);
            } else {
                text::add_shadowed_by_height(text, at, size, color_at, shadow, 2.0 * scale);
            }
        };
        paint(&label, pos, em);
        if popup.hits > 1 {
            let top = f32::from_bits(DIGIT_TOP.load(Ordering::Relaxed)) * em;
            let at = [pos[0] + size[0] + em * 0.04, pos[1] + top - em * 0.12];
            paint(&format!("x{}", popup.hits), at, unit * COMBO_SIZE);
        }
        if let Some(mark) = mark.filter(|_| marked) {
            let top = f32::from_bits(DIGIT_TOP.load(Ordering::Relaxed)) * em;
            let bottom = f32::from_bits(DIGIT_BOTTOM.load(Ordering::Relaxed)) * em;
            let side = bottom - top;
            let center = [pos[0] - em * 0.08 - side / 2.0, pos[1] + (top + bottom) / 2.0];
            let bands = if popup.layers.is_empty() { 1 } else { MARK_BANDS };
            draw_mark(ui, mark, center, side, bands, color_at);
        }
    }
}

/// `Crit.png` has a margin around the splat: 16 of its 256 pixels on each side.
const MARK_MARGIN: f32 = 256.0 / 224.0;

/// Horizontal slices of the mark when its color changes with the height.
const MARK_BANDS: usize = 12;

/// The mark of a critical hit or a burst, its splat `side` wide around `center`, cut into `bands`
/// slices each in the color `color_at` gives for its height (0 at the bottom of the splat, 1 at the top).
fn draw_mark(
    ui: &Ui,
    texture: TextureId,
    [x, y]: [f32; 2],
    side: f32,
    bands: usize,
    color_at: impl Fn(f32) -> [f32; 4],
) {
    let half = side * MARK_MARGIN / 2.0;
    let draw = ui.get_foreground_draw_list();
    for band in 0..bands {
        let (from, to) = (band as f32 / bands as f32, (band + 1) as f32 / bands as f32);
        // Where the middle of the slice is within the splat, which the margin frames in the image.
        let middle = (((from + to) / 2.0 - 0.5) * MARK_MARGIN + 0.5).clamp(0.0, 1.0);
        draw.add_image(
            texture,
            [x - half, y - half + 2.0 * half * from],
            [x + half, y - half + 2.0 * half * to],
        )
        .uv_min([0.0, from])
        .uv_max([1.0, to])
        .col(color_at(1.0 - middle))
        .build();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn critical_keeps_full_overkill_damage_and_element_color() {
        let mut tracker = Tracker::default();
        tracker.hit(&Event::test_hit());
        assert_eq!(tracker.popups.len(), 1);
        let popup = &tracker.popups[0];
        assert_eq!(popup.label, "574");
        assert_eq!(popup.kind, Kind::Holy);
        assert!(popup.critical);
        assert_eq!(popup.anchor, [0.0, 1.5, 10.0]);
    }

    fn tick(kind: i8, loss: i32, causer: i64) -> EffectTick {
        EffectTick {
            at: Instant::now(),
            owner: 1,
            handle: Some(7),
            max: 1000,
            causer,
            kind,
            changes: [-loss, 0, 0],
        }
    }

    #[test]
    fn the_effects_of_the_player_show_by_kind() {
        let mut tracker = Tracker::default();
        tracker.show_effect(7, Kind::Poison, 343, [0.0; 3]);
        tracker.show_effect(7, Kind::Bleed, 50595, [0.0; 3]);
        assert_eq!(tracker.popups.len(), 2);
        assert_eq!(tracker.popups[0].label, "343");
        assert!(tracker.popups[0].dot && !tracker.popups[0].burst());
        assert_eq!(tracker.popups[1].label, "50595");
        assert!(tracker.popups[1].burst() && !tracker.popups[1].dot);
        assert_ne!(tracker.popups[0].sequence, tracker.popups[1].sequence);
    }

    #[test]
    fn the_effects_of_others_and_heals_are_left_out() {
        let mut tracker = Tracker::default();
        let player = Some(9);
        tracker.effect(&tick(20, 343, 5), player);
        tracker.effect(&tick(20, 343, -1), player);
        tracker.effect(&tick(-2, 482, 9), player);
        let mut heal = tick(20, 343, 9);
        heal.changes[0] = 343;
        tracker.effect(&heal, player);
        let mut own = tick(20, 343, 9);
        own.handle = player;
        tracker.effect(&own, player);
        assert!(tracker.popups.is_empty());
    }

    #[test]
    fn the_ticks_of_effects_are_not_capped_like_hits() {
        let mut tracker = Tracker::default();
        for _ in 0..20 {
            tracker.show_effect(7, Kind::Physical, 325, [0.0; 3]);
        }
        assert_eq!(tracker.popups.len(), 20);
        let mut event = Event::test_hit();
        for _ in 0..20 {
            tracker.hit_with(&event, false);
        }
        assert_eq!(tracker.popups.iter().filter(|p| !p.dot).count(), 5);
        event.target = Some(7);
        tracker.hit_with(&event, false);
        // The 20 ticks of the effects, the 5 hits kept for the first target, the hit on this one.
        assert_eq!(tracker.popups.len(), 26);
    }

    #[test]
    fn crowds_and_rapid_hits_have_bounded_popup_counts() {
        let mut tracker = Tracker::default();
        let mut event = Event::test_hit();
        for _ in 0..20 {
            tracker.hit(&event);
        }
        assert_eq!(tracker.popups.len(), 5);
        for handle in 3..200 {
            event.target = Some(handle);
            tracker.hit(&event);
        }
        assert_eq!(tracker.popups.len(), MAX_POPUPS);
    }

    #[test]
    fn projection_tracks_the_camera_and_rejects_points_behind_it() {
        let mut camera = Camera {
            rows: [
                [1.0, 0.0, 0.0, 0.0],
                [0.0, 1.0, 0.0, 0.0],
                [0.0, 0.0, 1.0, 0.0],
                [0.0, 0.0, 0.0, 1.0],
            ],
            fov: std::f32::consts::FRAC_PI_2,
            aspect: 2.0,
            near: 0.1,
        };
        let area = [0.0, 50.0, 1000.0, 500.0];
        assert_eq!(camera.project([0.0, 0.0, 10.0], area), Some(([500.0, 300.0], 10.0)));
        assert_eq!(camera.project([0.0, 0.0, -10.0], area), None);
        camera.rows[3][0] = 10.0;
        assert_eq!(camera.project([10.0, 0.0, 10.0], area), Some(([500.0, 300.0], 10.0)));
        assert_eq!(camera.project([0.0, 0.0, 10.0], area), Some(([250.0, 300.0], 10.0)));
    }

    #[test]
    fn split_damage_uses_the_largest_element_not_physical_strength() {
        assert_eq!(element(Some([151.0, 0.0, 0.0, 0.0, 155.0])), Kind::Holy);
        assert_eq!(element(Some([500.0, 0.0, 10.0, 0.0, 0.0])), Kind::Fire);
        assert_eq!(element(Some([500.0, f32::NAN, 0.0, 0.0, 0.0])), Kind::Physical);
    }

    #[test]
    fn hits_in_quick_succession_grow_one_number_when_merging() {
        let mut tracker = Tracker::default();
        let mut event = Event::test_hit();
        event.hit_kind = Some(0);
        event.damage = Some(40);
        tracker.hit_with(&event, true);
        event.sequence = 1;
        event.damage = Some(60);
        event.hit_kind = Some(2);
        tracker.hit_with(&event, true);
        assert_eq!(tracker.popups.len(), 1);
        assert_eq!(tracker.popups[0].label, "100");
        assert_eq!(tracker.popups[0].hits, 2);
        assert!(tracker.popups[0].critical);
        event.target = Some(9);
        tracker.hit_with(&event, true);
        assert_eq!(tracker.popups.len(), 2);
        tracker.hit_with(&event, false);
        assert_eq!(tracker.popups.len(), 3);
    }

    #[test]
    fn parts_give_physical_what_the_elements_leave_of_the_total() {
        let parts = parts(126, Some([0, 0, 0, 51])).unwrap();
        assert_eq!(parts, [(Kind::Physical, 75), (Kind::Holy, 51)]);
        assert_eq!(super::parts(40, Some([0, 40, 0, 0])).unwrap(), [(Kind::Magic, 40)]);
        assert!(super::parts(0, Some([0; 4])).is_none());
        assert!(super::parts(40, None).is_none());
    }

    #[test]
    fn each_type_keeps_its_color_over_its_share_from_the_bottom() {
        let layers = [([1.0; 3], 0.5), ([0.0, 0.0, 1.0], 0.3), ([1.0, 0.0, 0.0], 0.2)];
        assert_eq!(layer_color(&layers, 0.0), [1.0; 3]);
        assert!(
            layer_color(&layers, 0.65)
                .iter()
                .zip([0.0, 0.0, 1.0])
                .all(|(a, b)| (a - b).abs() < 1e-3)
        );
        assert_eq!(layer_color(&layers, 1.0), [1.0, 0.0, 0.0]);
        let between = layer_color(&layers, 0.5);
        assert!(between[0] > 0.0 && between[0] < 1.0);
    }
}
