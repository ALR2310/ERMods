# Floating damage numbers

Status: **Confirmed on 2.7.2.0** for physical and magic hits and CRIT (screenshots against HP loss). Fire,
lightning, holy and the numbers of the effects (poison, scarlet rot, the bleed and frost bursts) follow the same
code paths but were only checked in the log, not by eye.
Conventions: [research notes](../../../docs/research/README.md).

Your own hits, and the poison, scarlet rot, bleed and frost damage you do, float above the enemy as numbers
colored by damage type, then fall and fade (`[DamageNumbers]` in `JourneyHUD.toml`).

## Where the numbers come from

**Confirmed.** The game's hit function is called as `(ctx, attacker, hit_info, arg4, flag)`; `[ctx + 8]` is the
target, and `attacker` is the attacking character (the same call AutoRegen observes, see
[AutoRegen per-hit](../../autoregen/docs/per_hit.md)). `damage/probe.rs` redirects that one `call` found by AOB
(`CALL_AOB`), copies what it needs into a bounded lock-free ring and forwards all five arguments, so a prologue hook
from another mod still chains. Nothing else runs in the hook; the frame task drains the ring.

| Read | Where | Use |
|---|---|---|
| Damage | `hit_info + 0x228` (i32) | the number; it keeps overkill (574 shown on a 219 HP target) |
| Components | `hit_info + 0x00`, five floats: physical, magic, fire, lightning, holy | color: the largest element input, physical only when none is present |
| Critical | `hit_info + 0xD9 == 2` | a splash mark at the left of the number in its color (backstab, riposte), size x2, twice as long |
| Position | `chr_ctrl.physics_model_matrix` row 3 | anchor, raised 1.5 m |

Only hits whose attacker is the local player create a number. Summons, other players and projectiles whose
attacker is not the player are not covered.

The damage is the pre-HP-delta request: it passes through the Defence curve and the target's cut rates, so it
matches the HP loss on ordinary hits (checked on a Holy weapon: 124 and 126 against HP loss 124 and 126). The
per-element integers at `+0x234..+0x240` must not be summed to the total (they are truncated one by one).

## Element after the calculation

**Confirmed from the decompilation on 2.7.2.0.** The hit function (`0x4478F0`) calls the damage calculation
(`0x685CD0`) with an output array of five floats, the result after the Defence curve and the target's cut rates,
in the order physical, magic, fire, lightning, holy. That array is a local variable of the hit function: only the
total and four of the five values reach the hit record, truncated to integers:

| Hit record | Value |
|---|---|
| `+0x228` | total: the sum of the five floats after later adjustments, times the scale at `+0x244`, as an integer |
| `+0x234` | fire (array index 2) |
| `+0x238` | magic (index 1) |
| `+0x23C` | lightning (index 3) |
| `+0x240` | holy (index 4) |

`Style` picks what to show: `Gradient` (the total, its glyphs colored from the bottom by the five reduced values in the
order physical, magic, fire, lightning, holy, each over its share of the height, by per-vertex colors; physical is
estimated as below) or `Split` (one number per value above zero, flying out one by one in a fan, each in its type's color). A critical hit always shows one number, stacked as in `Gradient`. Without the element fields (unreadable record) every style falls back to the input rule. Poison, rot and bursts keep their own colors.

The physical result (index 0) is not stored. The five floats at `hit_info +0x00` that the mod reads are the
attack inputs, before the reduction. The color comes from those inputs (the larger element input), not from the
reduced values. The reduced elements could be read from `+0x234..+0x240` after the call, and physical estimated as
the total minus them, but the truncation of each value and the later adjustments of the total make that off by a
few points (a checked hit: 74 + 51 = 125 against a total of 126), so it is not done.

## Merging quick hits

With `Combo` on, a hit on a target that was hit less than a second before is added to that hit's number instead of
making a new one: the total, the per-type parts and CRIT (if any of the hits was) are summed, and the number
rises back to its start in 0.2 s while it swells and brightens, and then lasts a full `Duration` after the last one. Bursts stay apart.
A number of two or more hits has a counter (`x3`) at the top right, in the number's colors; in `Split`
only the first number has it.
The summing follows the style: `Split` grows each type's number, `Gradient` the share of each color.

## Colors

`[DamageNumbers.Colors]` holds one `#RRGGBB` per kind (physical, the four elements, bleed, frost, poison, rot, burn), read into
atomics by `settings::load` so a config reload applies them. The defaults live only in the template; a value that is not
six hex digits is logged and replaced by the template's. The gradient, the CRIT mark and the combo counter take their color from
these, so they follow too.

## Effects

**Confirmed on 2.7.2.0, also with a third-party overhaul mod (log of `Logging.DamageProbe`, poison, rot, bleed and frost seen).** HP that
effects take does not pass through the hit call. The game adds it up once per frame and character in one function
(`sub_1404F8BB0`, called from `sub_1404413B0`, `damage/effects.rs`): from the effects on the character it gives the
HP change of the frame, the handle the loss is credited to, and the `spAttribute` of the effect behind it. The one
call of it is found by AOB (`CALL_AOB`, unique on 2.6.2.0 to 2.7.2.0), redirected like the hit call, and the result
is copied into a ring (character, credited handle, kind, change).

| Kind (`spAttribute`) | Effect | Shown as |
|---|---|---|
| 20 | poison | small number, at each tick, about once a second (343 on a 481 195 HP enemy) |
| 21 | scarlet rot | the same (599 on a 324 996 HP enemy) |
| 22 | blood loss | a big number, once (50 595: the burst) |
| 23 | frostbite | a big number, once (33 704: the burst) |
| -2 | burns (Black Flame, Soul Burn, Destined Death) | small number in the `Burn` color (dark gray), at each tick, ten a second |

These are shown when the loss is negative, the credited handle is the player's (the same value as the `attacker`
of the player's hits) and the character is not the player. The value is what the game took, with no HP read and
no timing rule, so a hit in the same frame, a second hit within 150 ms or poison on the target no longer change
it. Each tick is its own number: there is no fixed timer, a number shows when the game computes the tick.

**Other effect damage.** Every other loss that effects take from a hostile enemy (`radar::enemies::is_hostile`, the
classification of the minimap dots) shows as a small number in the color of physical damage, because neither whose
it is nor of what kind is known: the ailments credited to someone else, and the effects with no `kind` that are not
credited to the player. Each tick is its own number, as often as the game computes it. `EffectDamage` turns all
of this damage that does not come from a hit (the four ailments, burns and the rest) on or off together; your hits
always show. Seen in the log:
- **Burns** (Black Flame, Destined Death, a soul burn of the third-party mod): `kind` is `-2` (they have no
  `spAttribute` of the ailments), the loss is credited to the player, and they tick ten times a second: 482 on a
  481 195 HP enemy for Scouring Black Flame (0.1% of max HP plus 1, `ChangeHpRate 0.1`, `ChangeHpPoint 1`), 325 and
  then 293 for Destined Death, whose second effect (`6865`) lowers the max HP of the target by a tenth for 45 s,
  and 17 on a 7 410 HP enemy for the soul burn. The effects on the target tell them apart: `1623000` and the
  other ids of Black Flame, `6850` and `6865` for Destined Death, `1620000` while the soul burn ticks. Any `-2`
  loss credited to the player is a burn of some kind, so they share one color (`Burn`): the third-party mod's Soul Burn and Destined Death are rebalanced versions of Black Flame.
- **The procs of death blight and madness**: `kind` is `-2` and the loss is credited to nobody, 0.1 to 0.2 s after a
  hit of the player resets that buildup (index 3 and 6 of the remaining buildup) and after two hits with no damage
  (the target on itself, the target on the player): 6% of the max HP for death blight with the third-party jewel
  that adds it (444 on 7 410 HP; without the jewel the proc took no HP), 474 for madness. The value is the
  game's; only that it is the player's is a guess from the timing.
- **Sleep** takes no HP.

Both of the first two now show as other effect damage (a burn as a number per tick, the procs of death blight and
madness as one small number each). When two effects tick in one frame the game adds them and `kind` is the one of the larger, so the number is the
sum in the color of that effect.

## Drawing

Size is `FontSize` (42 px at 1080p) times `Scale`; CRIT and bursts are twice that, effect ticks half, the hit counter 0.4, in Impact (loaded from the Windows fonts folder,
the default font when it is absent), and grows or shrinks with the distance from the camera, full size at 15 m,
between three quarters and one and a half times (a burst never gets smaller than full size). Each number shrinks to 40% while it falls and fades out; a CRIT or a burst stays twice as long.

The frame task owns popup lifetimes and hands copied data (popups, camera) to the render thread, which never
touches a character pointer. The anchor stays in the world and is projected each frame with `CSCamera.pers_cam_1`
(view matrix, `fov`, `aspect`), fitted to the picture inside the window for non-16:9 displays. The number falls in screen space. At most 5 numbers of hits and bursts per enemy (the ticks of effects are not counted) and 96 in total; a loading screen, a cutscene, a stale
frame or a new player character clears them, and the HUD toggle hides them.

The game's own damage number on ordinary enemy tags (`Damage` clip, `hud/enemy_tags.rs`) is moved off the picture when
`HUD.EnemyDamage` is false, keeping the health bar when `HUD.EnemyHPBar` is set; `EnemyHPBar = false`
hides both. The key does not depend on `[DamageNumbers]`. The movie is edited at start, so changing that needs a
restart.

The splash mark is `assets/Crit.png`, drawn for the mod from the outline of a MOBA critical mark (a white image with a
black outline, tinted with the number's color; not taken from the game).

## Open risks

- The anchor is the target's position before the hit, not the impact point; fast targets can look off.
- Numbers from rapid hits can overlap, more so with a long `Duration`.
- Not tested: the effect numbers by eye, fire, lightning and holy numbers by eye, lethal CRIT, AutoRegen loaded
  together, ultrawide.
- `CALL_AOB`, the effect call pattern and the hit record offsets were only checked on 2.7.2.0 (the two patterns are
  unique on 2.6.2.0 to 2.7.2.0). When one is missing that capture stays off and the log says so.
- Another mod that rewrites the same call site would be chained by its own detour, not by this one.

## Diagnostics

`Logging.DamageProbe` logs every hit (`Damage probe:`), the popup it made (`Damage popup:`) and the HP change of
the target for two seconds after it. It also logs each effect result (`HP effects:`: the change, the max HP, the
handle it is credited to, the kind) and each HP the game sets on a character (`HP write:`), with the calls on the
stack that led to it (`damage/hp_probe.rs`, `sub_1404374E0`): a hit comes through `436BF9`, the effects through `4415B3`
(RVAs on 2.7.2.0). The probe is off by default and writes a lot at an area load.
