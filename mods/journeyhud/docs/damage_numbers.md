# Floating damage numbers

Status: **Confirmed on 2.7.2.0** for physical and magic hits and CRIT (screenshots against HP loss). Fire,
lightning, holy and the bleed/frost bursts follow the same code path but were only checked in the log, not by eye.
Conventions: [research notes](../../../docs/research/README.md).

Your own hits float above the enemy as numbers colored by damage type, then fall and fade
(`[DamageNumbers]` in `JourneyHUD.toml`).

## Where the numbers come from

**Confirmed.** The game's hit function is called as `(ctx, attacker, hit_info, arg4, flag)`; `[ctx + 8]` is the
target, and `attacker` is the attacking character (the same call AutoRegen observes, see
[AutoRegen per-hit](../../autoregen/docs/per_hit.md)). `damage_probe.rs` redirects that one `call` found by AOB
(`CALL_AOB`), copies what it needs into a bounded lock-free ring and forwards all five arguments, so a prologue hook
from another mod still chains. Nothing else runs in the hook; the frame task drains the ring.

| Read | Where | Use |
|---|---|---|
| Damage | `hit_info + 0x228` (i32) | the number; it keeps overkill (574 shown on a 219 HP target) |
| Components | `hit_info + 0x00`, five floats: physical, magic, fire, lightning, holy | color: the largest element input, physical only when none is present |
| Critical | `hit_info + 0xD9 == 2` | orange numbers with a jagged orange splash at the left (backstab, riposte), size x1.5, twice as long |
| HP | `chr + 0x190 -> [0] + 0x138` | HP before and after the call |
| Remaining buildup | `chr + 0x190 -> [+0x20] + 0x10`, seven ints | index 2 = bleed, 4 = frost |
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

`Mode` picks what to show: `Combined` (the total, its glyphs colored from the bottom by the five reduced values in the
order physical, magic, fire, lightning, holy, each over its share of the height, by per-vertex colors; physical is
estimated as below) or `Split` (one number per value above zero, flying out one by one in a fan; a critical hit always shows one number, as in `Combined`). Without the element fields (unreadable record) every mode falls back to the input rule.

The physical result (index 0) is not stored. The five floats at `hit_info +0x00` that the mod reads are the
attack inputs, before the reduction. The color comes from those inputs (the larger element input), not from the
reduced values. The reduced elements could be read from `+0x234..+0x240` after the call, and physical estimated as
the total minus them, but the truncation of each value and the later adjustments of the total make that off by a
few points (a checked hit: 74 + 51 = 125 against a total of 126), so it is not done.

## Merging quick hits

With `Merge` on, a hit on a target that was hit less than a second before is added to that hit's number instead of
making a new one: the total, the per-type parts and CRIT (if any of the hits was) are summed, and the number
rises back to its start in 0.2 s while it swells and brightens, and then lasts a full `Duration` after the last one. Bursts stay apart.
The summing follows the style: `Split` grows each type's number, `Combined` the share of each color.

## Bleed and frost bursts

**Inferred.** A proc does not go through the hit call. When one hit resets the target's bleed or frost buildup by
more than half its new value, the HP lost in the next 150 ms beyond the hit's own damage becomes a red or cyan
number. Discarded when: another positive hit lands on that target, the target has poison or rot (`stateInfo` 2 or
116), both buildups reset at once, the ring overflowed, or the target is gone. Procs that do not reset the buildup
by that margin are missed on purpose rather than guessed.

## Drawing

Size is 42 px at 1080p (63 px for CRIT and bursts) times `Scale`, in Impact (loaded from the Windows fonts folder,
the default font when it is absent), and grows or shrinks with the distance from the camera, full size at 8 m,
between half and one and a half times (a burst never gets smaller than full size). Each number shrinks to 40% while it falls and fades out; a CRIT or a burst stays twice as long.

The frame task owns popup lifetimes and hands copied data (popups, camera) to the render thread, which never
touches a character pointer. The anchor stays in the world and is projected each frame with `CSCamera.pers_cam_1`
(view matrix, `fov`, `aspect`), fitted to the picture inside the window for non-16:9 displays. The number falls in screen space. At most 5 numbers per enemy and 96 in total; a loading screen, a cutscene, a stale
frame or a new player character clears them, and the HUD toggle hides them.

The game's own damage number on ordinary enemy tags (`Damage` clip, `enemy_tags.rs`) is moved off the picture when
the feature is on, keeping the health bar when `HUD.EnemyHPBar` is set. The movie is edited at start, so changing
that needs a restart.

## Open risks

- The anchor is the target's position before the hit, not the impact point; fast targets can look off.
- Numbers from rapid hits can overlap, more so with a long `Duration`.
- Not tested: fire, lightning and holy numbers by eye, lethal CRIT, bleed/frost with and without poison, AutoRegen
  loaded together, ultrawide.
- `CALL_AOB` and the hit record offsets were only checked on 2.7.2.0. When the pattern is missing the capture stays
  off and the log says so.
- Another mod that rewrites the same call site would be chained by its own detour, not by this one.

## Diagnostics

`Logging.DamageProbe` logs every hit (`Damage probe:`), the popup it made (`Damage popup:`) and the HP change of
the target for two seconds after it, which is how the burst rule above was found.
