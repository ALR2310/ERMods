# Damage numbers above enemies (research before building)

Status: **Hypothesis.** Nothing here was tried in game. What exists: the layouts of the game's own damage-number
data (from the typed bindings, `eldenring` fromsoftware-rs), one function family found in IDA on 2.7.2.0, and what
could be learned about other tools. Conventions: [research notes](../../../docs/research/README.md).

## Question

Show the damage the player deals as numbers that float up from the enemy that took it, the way damage
indicators work in block-game mods: one number per hit, near the enemy, rising and fading, sized or colored by how
big the hit was. Which data can feed it, where does it get a screen position, and what does it cost?

## What we have

- **Health of the enemies we already read.** `target.rs` reads `chr.modules.data.hp` and `max_hp` for the locked-on
  enemy and for the bosses on a health bar; `radar/enemies.rs` walks the hostile enemies in range (`chr_sets`, type,
  team, `threatLv`) and `ui.rs` redraws every frame.
- **The camera.** `radar/mod.rs` reads `CSCamera.pers_cam_1.matrix` (a view matrix) for the minimap's turning;
  `CSCam` also has `fov`, `aspect_ratio`, `near_plane`, `far_plane`. Enough to project a world position, if the
  matrix layout is confirmed (we only use its axis vectors so far).
- **World positions.** `chr.chr_ctrl.physics_model_matrix` row 3 is the position (`target::offset_from`).
- **An overlay with text.** hudhook draw list, `text::add_shadowed`, the 1080p layout with `res` scale, and a fade
  shared with the game's HUD (`fade`, `PANEL_ALPHA`).
- **The game's own numbers can be hidden.** `enemy_tags.rs` moves the `HP` and `Damage` clips of the enemy tags off
  the picture (`HUD.EnemyHPBar`), so a floating number would not double the game's own.

## What the game has

- **A number already exists, accumulated.** The enemy tag above a hit enemy and the boss health bar show the total
  damage taken in the last moment, as one number that grows with each hit (the number at the right end of a boss's
  bar). Data, from the typed layouts:

  | Where | Field | Meaning |
  |---|---|---|
  | `CSFeManImp.enemy_chr_tag_displays[8]` (entries of 0x40) | `damage_taken` (+0x24) | hp lost, the number of the tag |
  | | `pre_damage_hp` (+0x28) | hp before the damage (for the bar's drain) |
  | | `last_damage_time_delta` (+0x30) | seconds since the last damage; at 1.5 the number is removed |
  | | `last_update_time_delta` (+0x2C) | at 1.5 the whole tag is removed |
  | | `screen_pos` (+0x10, `F32Vector4`) | the screen position the game projected for the tag |
  | | `field_ins_handle` (+0x00), `is_visible` (+0x34) | which enemy, and whether it is on screen |
  | `CSFeManImp.boss_health_displays[3]` (0x20, at +0x5BF0, confirmed on all four builds) | `damage_taken` (+0x10) | the same total for a boss bar |
  | `CSFeManImp.damage_number_decay_time` | `f32` | seconds the boss bar's number stays; enemy tags use a constant 1.5 |

  The offsets of the enemy tags are derived from the struct order (they sit 0x200 below the boss entries, so at
  about +0x59F0) and have **not** been read in game; the size of `CSFeManImp` is asserted by the bindings' tests.
- **Only a few enemies have a tag.** The array has 8 entries: the enemies that were hit or locked on recently, not
  every enemy. A kill in one hit may remove the tag before it is ever shown.
- **Synced health.** `ChrSyncHealthUpdate { current_hp, damage_taken }` carries the damage to characters whose
  state comes over the network (co-op).
- **A damage module.** `CS::CSChrDamageModule` has a vtable at RVA 0x2A3BA90 on 2.7.2.0 (13 slots). Two of its slots
  are large and look like the place where a hit is applied: slot 7 (0x445650, a hit and a second argument at
  +606 checked first) and slot 9 (0x444E30). Not read through: the decompilation names are noise (the first
  argument is typed wrong). **Nothing is known yet about the hit record**: where the final damage, the attacker, the
  types (physical, magic, fire, lightning, holy) and the critical flags sit.

## What others have

Read from search results only; the pages of two of them were blocked (HTTP 403) and **not read**:

- A Nexus mod for Seamless Co-op (id 10461) says it pops up damage numbers for the player and allies, colored by
  status effect, with a damage meter. How it gets the numbers is unknown.
- A Nexus damage and DPS counter (id 10264) is an overlay with per-player DPS and session totals, loaded through me3.
- An open-source trainer's README describes damage popups "based on real HP changes": 5 seconds on screen, rising
  slowly and fading, at most 5 per target stack, 10 tiers of size and color by amount, and a fallback so the last
  hit of an enemy that vanishes at once is still shown. Its popup code is not in the public tree.
- A posture-bar mod places bars on the enemy tags' `screen_pos` (with offsets and screen-edge thresholds), which
  suggests that `screen_pos` is usable as a position.
- Several tools go the other way and replace or hide the game's damage number.
- No tool was found that shows per-hit numbers with the type of the damage; the one that colors by "status effect"
  is the closest.

## Ways to get the numbers

| | Source | Gives | Misses |
|---|---|---|---|
| A | Difference of `hp` of the enemies we track, each frame | A number for any damage source; no hook | The overkill (hp stops at 0); two hits in one frame merge; heals look like damage; no type, no crit, no attacker; an enemy gone the next frame loses its last number |
| B | Difference of the tags' `damage_taken` (and `boss_health_displays`) | The game's own accumulation, the screen position, the enemy handle | At most 8 enemies; the number resets after 1.5 s; same limits as A for hits in one frame |
| C | A hook on the hit in `CSChrDamageModule` | The real amount per hit (overkill too), maybe the attacker, the types and the critical | Needs the hit record reversed and an AOB that holds on every build; a hook in the game's damage path is the riskiest thing a mod does, and with Seamless Co-op the remote hits may go another way |
| D | The synced health update | Damage to synced characters | Only for characters synced from other machines |

A and B need no new hook and use fields the mod already reads or can read through the typed layouts; C is the only
way to get types and overkill.

## Placing a number

- **From the tag**: `screen_pos` is the game's own projection, in game screen coordinates (to confirm: 1920x1080
  or the window size). No matrix math, but only for the 8 tagged enemies.
- **From the world**: the enemy's position plus a height, through the view matrix, `fov` and `aspect_ratio`. Works
  for any enemy, but the matrix convention, the projection and the height of the head per enemy size have to be worked
  out; the radar's use of the same matrix (right and forward vectors on the ground) shows its rows are the camera axes.
- Hide a number that is behind the camera or off screen; shift it so the numbers of one enemy do not overlap (a
  stack of at most a few, newest at the bottom).

## Design questions

- One number per hit or the running total of a combo? (The game already shows the total.)
- Rising and fading time, size and color by amount (tiers) or by type (needs C), a limit of numbers per enemy and in
  all, a different look for the killing blow and for the critical.
- Which damage: only the player's, or allies and spirits too (needs the attacker, so C or D).
- Settings: on or off, size, duration, the key that hides them, and whether to hide the game's own number when ours is on.
- Cost: a few text draws per frame; the number of tracked enemies and the reading of their hp every frame.

## Proposed experiments

```text
question: do the enemy tags carry the damage and a usable screen position, and how do they behave on a kill?
game build: 2.7.2.0
mod build: a probe that logs the 8 enemy tags (handle, damage_taken, pre_damage_hp, deltas, screen_pos,
           is_visible) when one of them changes, and the hp of the locked-on enemy
entry state: a fight with an ordinary enemy near the player, the overlay on
exact player actions: one hit, two hits in a row, a hit that kills, a hit that overkills a weak enemy, a heal on
           an enemy if one is at hand, then the boss bar: a hit on a boss
control case: the same with `HUD.EnemyHPBar` off (the tag is hidden but the data should still be there)
expected observation: damage_taken grows by the hit; the tag disappears after 1.5 s; screen_pos follows the enemy
time limit and capture limit: 5 minutes, one log
cleanup: remove the probe
```

A second experiment, for C: find the hit record in `CSChrDamageModule` slot 7 (read the second argument's fields
at the point where hp is lowered), and log the amount, the attacker and the types for the same sequence of hits.

## Open risks

- The enemy tag offsets and the meaning of the fields are from the bindings, not from this game build.
- `screen_pos` may be in a different space (a 16:9 frame inside the window) than our overlay uses.
- A hook on the damage module could clash with other mods that hook it (damage-formula and difficulty mods); with
  Seamless Co-op the damage of other players arrives through another path.
- Numbers for allies and for damage the player did not deal may not be wanted; the sources A and B cannot tell them
  apart.
