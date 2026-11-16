//! The frame task owns popup lifetimes and attribution. Rendering reads only copied data.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use eldenring::cs::{CSCam, CSCamera, CSMenuManImp, FieldInsHandle, WorldChrMan};
use fromsoftware_shared::FromStatic;
use hudhook::imgui::{Context, FontId, FontSource, Ui};
use shared::memscan;

use crate::damage_probe::{Event, state};
use crate::{settings, target, text};

/// Pixel sizes at 1080p: a plain hit, and a critical hit or a burst.
const HIT_PX: f32 = 42.0;
const BIG_PX: f32 = 63.0;
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
/// A critical hit's numbers are this color instead of the type's.
const CRIT_COLOR: [f32; 3] = [0.96, 0.50, 0.22];
/// A number that takes another hit rises back to its start in `REWIND` seconds, and swells and
/// brightens for `POP` seconds.
const REWIND: f32 = 0.2;
const POP: f32 = 0.25;
const POP_SCALE: f32 = 0.35;
/// The font is rasterized once at this size, so the numbers stay sharp up to it.
const FONT_PX: f32 = 72.0;
/// A number is full size at this distance from the camera, smaller farther and larger nearer.
const FULL_SIZE_METERS: f32 = 8.0;
/// Hits on one target this close together grow one number (`DamageNumbers.Merge`).
const MERGE_WINDOW: Duration = Duration::from_millis(1000);
const BURST_WINDOW: Duration = Duration::from_millis(150);
const STALE: Duration = Duration::from_millis(500);
const MAX_POPUPS: usize = 96;
const MAX_PENDING: usize = 32;
const _: () = assert!(std::mem::size_of::<FieldInsHandle>() == 8);

/// Where the digits of the font start and end under the top of the line, as a fraction of its size.
static DIGIT_TOP: AtomicU32 = AtomicU32::new(0.17f32.to_bits());
static DIGIT_BOTTOM: AtomicU32 = AtomicU32::new(0.92f32.to_bits());
static LATEST: Mutex<Option<Snapshot>> = Mutex::new(None);

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    Physical,
    Magic,
    Fire,
    Lightning,
    Holy,
    Bleed,
    Frost,
}

impl Kind {
    fn color(self) -> [f32; 3] {
        match self {
            Self::Physical => [0.95, 0.95, 0.90],
            Self::Magic => [0.345, 0.553, 0.941],
            Self::Fire => [1.0, 0.486, 0.137],
            Self::Lightning => [1.0, 0.925, 0.275],
            Self::Holy => [1.0, 0.878, 0.647],
            Self::Bleed => [0.953, 0.086, 0.082],
            Self::Frost => [0.702, 0.851, 1.0],
        }
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

// A large reset within the hit is distinct from the gradual recovery between hits.
fn burst(before: Option<[i32; 7]>, after: Option<[i32; 7]>) -> Option<Kind> {
    let (before, after) = before.zip(after)?;
    let reset = |i: usize| {
        before[i] >= 0
            && after[i] > 0
            && i64::from(after[i]) - i64::from(before[i]) > i64::from(after[i]) / 2
    };
    match (reset(2), reset(4)) {
        (true, false) => Some(Kind::Bleed),
        (false, true) => Some(Kind::Frost),
        _ => None,
    }
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
    /// Whether a later hit was added, and how far through its life the number was then.
    merged: bool,
    from: f32,
    /// Where a split number flies, from the left (below 0) to the right.
    fan: f32,
    /// Index in a split hit: later numbers leave later.
    row: usize,
    critical: bool,
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
    merged: bool,
    from: f32,
}

struct Pending {
    popup: Popup,
    hp: i32,
    loss: i64,
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
    pending: Vec<Pending>,
    combos: Vec<Combo>,
    last: Option<Instant>,
    player: usize,
}

impl Tracker {
    pub fn update(&mut self, events: &[Event], dropped: bool) {
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
            self.pending.clear();
            self.combos.clear();
        }
        if active && !changed {
            self.popups.retain(|p| p.at.elapsed().as_secs_f32() < p.lasts());
            for event in events {
                self.hit(event);
            }
            if dropped {
                self.pending.clear();
            }
            self.follow();
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
            && self.popups.iter().filter(|p| p.handle == popup.handle && p.row == 0).count() >= 5
            && let Some(oldest) =
                self.popups.iter().find(|p| p.handle == popup.handle).map(|p| p.sequence)
        {
            self.popups.retain(|p| p.handle != popup.handle || p.sequence != oldest);
        }
        if self.popups.len() == MAX_POPUPS {
            self.popups.remove(0);
        }
        self.popups.push(popup);
    }

    fn hit(&mut self, event: &Event) {
        self.hit_with(event, settings::damage_merge());
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

    /// Puts the numbers of `combo` on screen, and returns the plain one the bursts are based on.
    fn show(&mut self, combo: &Combo, anchor: [f32; 3]) -> Popup {
        let kind = combo.parts.iter().max_by_key(|p| p.1).map_or(Kind::Physical, |p| p.0);
        let color_of = |kind: Kind| if combo.critical { CRIT_COLOR } else { kind.color() };
        let popup = Popup {
            handle: combo.handle,
            anchor,
            label: combo.total.to_string(),
            kind,
            color: color_of(kind),
            layers: Vec::new(),
            merged: combo.merged,
            from: combo.from,
            fan: 0.0,
            row: 0,
            critical: combo.critical,
            sequence: combo.sequence,
            at: combo.last,
        };
        match settings::damage_mode() {
            settings::DamageMode::Split if !combo.critical => {
                let count = combo.parts.len();
                for (row, &(kind, value)) in combo.parts.iter().enumerate() {
                    self.push(Popup {
                        label: value.to_string(),
                        kind,
                        color: color_of(kind),
                        row,
                        fan: row as f32 - (count - 1) as f32 / 2.0,
                        critical: popup.critical && row == 0,
                        ..popup.clone()
                    });
                }
            }
            settings::DamageMode::Combined | settings::DamageMode::Split => {
                let sum = combo.parts.iter().map(|p| p.1).sum::<i32>().max(1) as f32;
                let layers = if combo.critical || combo.parts.len() < 2 {
                    Vec::new()
                } else {
                    combo.parts.iter().map(|&(k, v)| (k.color(), v as f32 / sum)).collect()
                };
                self.push(Popup { layers, ..popup.clone() });
            }
        }
        popup
    }

    fn hit_with(&mut self, event: &Event, merge: bool) {
        let Some(handle) = event.target else { return };
        // Any subsequent positive hit makes attribution ambiguous, including another
        // player's hit. Zero-damage parries and background events do not erase a proc.
        if event.damage.is_some_and(|n| n > 0)
            || event.before.hp.zip(event.after.hp).is_some_and(|(before, after)| before > after)
        {
            self.pending.retain(|p| p.popup.handle != handle);
        }
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
                merged: false,
                from: 0.0,
            },
            merge,
        );
        let popup = self.show(&combo, anchor);
        let Some(kind) = burst(event.before.resist, event.after.resist) else { return };
        let (Some(before), Some(after)) = (event.before.hp, event.after.hp) else { return };
        if self.pending.len() == MAX_PENDING {
            self.pending.remove(0);
        }
        self.pending.push(Pending {
            popup: Popup {
                kind,
                color: kind.color(),
                layers: Vec::new(),
                merged: false,
                from: 0.0,
                critical: false,
                ..popup
            },
            hp: after,
            loss: (i64::from(before) - i64::from(after) - i64::from(damage)).max(0),
        });
    }

    fn follow(&mut self) {
        // SAFETY: singleton access fails while the world is unavailable.
        let world = unsafe { WorldChrMan::instance() }.ok();
        let mut ready = Vec::new();
        self.pending.retain_mut(|pending| {
            let raw = pending.popup.handle.to_ne_bytes();
            let handle = memscan::read_safe::<FieldInsHandle>(raw.as_ptr() as usize);
            let chr =
                world.zip(handle).and_then(|(world, handle)| target::chr_by_handle(world, handle));
            let Some(chr) = chr else { return false };
            // Poison and rot tick outside the intercepted hit. Reject them rather
            // than guessing how much of a coinciding HP delta belongs to the burst.
            if chr.special_effect.entries().any(|entry| {
                // SAFETY: the live effect entry owns this param row for this frame.
                entry
                    .param_data
                    .is_some_and(|row| matches!(unsafe { row.as_ref() }.state_info(), 2 | 116))
            }) {
                return false;
            }
            let Some(hp) = state(std::ptr::from_ref(chr) as usize).hp else { return false };
            if hp > pending.hp {
                return false;
            }
            pending.loss += i64::from(pending.hp) - i64::from(hp);
            pending.hp = hp;
            if pending.popup.at.elapsed() < BURST_WINDOW && hp > 0 {
                return true;
            }
            if pending.loss > 0 {
                let mut popup = pending.popup.clone();
                popup.label = pending.loss.to_string();
                popup.at = Instant::now();
                ready.push(popup);
            }
            false
        });
        for popup in ready {
            self.push(popup);
        }
    }
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

pub fn draw(ui: &Ui, area: [f32; 4], alpha: f32, font: Option<NumberFont>) {
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
        let near = (FULL_SIZE_METERS / depth).clamp(if popup.burst() { 1.0 } else { 0.5 }, 1.5);
        let scale = scale * near;
        let side = if popup.fan == 0.0 {
            (popup.sequence.wrapping_add(usize::from(popup.burst())) % 5) as f32 - 2.0
        } else {
            0.0
        };
        pos[0] += side * (30.0 + 45.0 * t) * scale;
        pos[0] += popup.fan * FAN_PX * (0.6 + 0.9 * t) * scale;
        pos[1] += FALL_PX * t * t * scale;
        let em = if popup.critical || popup.burst() { BIG_PX } else { HIT_PX }
            * scale
            * (1.0 - SHRINK * t)
            * (1.0 + POP_SCALE * pulse);
        let label = popup.label;
        let size = text::measure(ui, &label, em);
        pos[0] -= size[0] * 0.5;
        pos[1] -= size[1] * 0.5;
        let a = alpha * (1.0 - t);
        let flash = |c: f32| c + (1.0 - c) * 0.6 * pulse;
        let [r, g, b] = popup.color.map(flash);
        let shadow = [0.0, 0.0, 0.0, a];
        if popup.layers.is_empty() {
            text::add_shadowed(&label, pos, em, [r, g, b, a], shadow, 2.0 * scale);
        } else {
            let color_at = |height: f32| {
                let [r, g, b] = layer_color(&popup.layers, height).map(flash);
                [r, g, b, a]
            };
            text::add_shadowed_by_height(&label, pos, em, color_at, shadow, 2.0 * scale);
        }
        if popup.critical {
            let top = f32::from_bits(DIGIT_TOP.load(Ordering::Relaxed)) * em;
            let bottom = f32::from_bits(DIGIT_BOTTOM.load(Ordering::Relaxed)) * em;
            let mark = bottom - top;
            let center = [pos[0] - em * 0.08 - mark / 2.0, pos[1] + (top + bottom) / 2.0];
            draw_burst(ui, center, mark, a);
        }
    }
}

/// The outline of the critical mark in a unit square, traced from the art it copies: a splat whose
/// upper left is jagged around one long spike, and whose right and bottom sides are straight.
const SPLASH: [[f32; 2]; 14] = [
    [0.0, 0.0],
    [0.604, 0.378],
    [0.64, 0.36],
    [0.685, 0.189],
    [0.73, 0.342],
    [0.829, 0.505],
    [1.0, 0.099],
    [0.991, 0.991],
    [0.081, 1.0],
    [0.432, 0.874],
    [0.486, 0.829],
    [0.342, 0.73],
    [0.189, 0.685],
    [0.387, 0.613],
];
/// A point inside the outline that sees all of it, where the triangles of the fill meet.
const SPLASH_HUB: [f32; 2] = [0.82, 0.82];

/// The critical mark with its center at `center` and its body `size` wide: red-orange at the top left
/// to orange at the bottom right, with a black outline.
fn draw_burst(ui: &Ui, center: [f32; 2], size: f32, alpha: f32) {
    const BANDS: usize = 4;
    let draw = ui.get_foreground_draw_list();
    let origin = [center[0] - size / 2.0, center[1] - size / 2.0];
    let place = |unit: [f32; 2]| [origin[0] + unit[0] * size, origin[1] + unit[1] * size];
    let toward = |from: [f32; 2], to: [f32; 2], k: usize| {
        let f = k as f32 / BANDS as f32;
        [from[0] + (to[0] - from[0]) * f, from[1] + (to[1] - from[1]) * f]
    };
    // The fill is a fan from the hub, each triangle cut into bands so the color changes smoothly.
    for (i, &a) in SPLASH.iter().enumerate() {
        let b = SPLASH[(i + 1) % SPLASH.len()];
        for k in 0..BANDS {
            let (a0, b0) = (toward(SPLASH_HUB, a, k), toward(SPLASH_HUB, b, k));
            let (a1, b1) = (toward(SPLASH_HUB, a, k + 1), toward(SPLASH_HUB, b, k + 1));
            let middle = [(a0[0] + b1[0]) / 2.0, (a0[1] + b1[1]) / 2.0];
            let t = (((middle[0] + middle[1]) * 0.5 - 0.1) / 0.7).clamp(0.0, 1.0);
            let color = [0.74 + 0.22 * t, 0.25 + 0.31 * t, 0.19 + 0.06 * t, alpha];
            for corners in [[a0, a1, b1], [a0, b1, b0]] {
                let [p, q, r] = corners.map(place);
                draw.add_triangle(p, q, r, color).filled(true).build();
            }
        }
    }
    let black = [0.0, 0.0, 0.0, alpha];
    let width = (size * 0.06).max(2.0);
    for (i, &a) in SPLASH.iter().enumerate() {
        let b = SPLASH[(i + 1) % SPLASH.len()];
        draw.add_line(place(a), place(b), black).thickness(width).build();
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

    #[test]
    fn zero_damage_does_not_cancel_a_burst_but_another_attacker_does() {
        let mut tracker = Tracker::default();
        let mut event = Event::test_hit();
        event.before.resist = Some([51; 7]);
        let mut resist = [51; 7];
        resist[2] = 424;
        event.after.resist = Some(resist);
        tracker.hit(&event);
        assert_eq!(tracker.pending.len(), 1);
        event.damage = Some(0);
        event.before.hp = event.after.hp;
        event.own = false;
        tracker.hit(&event);
        assert_eq!(tracker.pending.len(), 1);
        event.damage = Some(50);
        tracker.hit(&event);
        assert!(tracker.pending.is_empty());
        assert_eq!(tracker.popups.len(), 1);
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

    #[test]
    fn burst_requires_one_large_reset_in_the_same_hit() {
        let mut before = [100; 7];
        let mut after = before;
        after[2] = 101;
        assert_eq!(burst(Some(before), Some(after)), None);
        before[2] = 51;
        after[2] = 424;
        assert_eq!(burst(Some(before), Some(after)), Some(Kind::Bleed));
        before[4] = 44;
        after[4] = 833;
        assert_eq!(burst(Some(before), Some(after)), None);
        after[2] = 50;
        assert_eq!(burst(Some(before), Some(after)), Some(Kind::Frost));
    }
}
