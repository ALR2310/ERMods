# What an enemy is weak to: damage types and statuses

Status: **Confirmed** in game on 2.7.2.0 with the Convergence pack: the values read from the game match the
param table for two enemies, and both rows of icons were drawn for common enemies and for bosses. Not checked:
other game versions, a boss that changes its `NpcParam` row between phases, co-op. Conventions:
[research notes](../../../docs/research/README.md). Code: `weakness.rs`, `ui.rs::draw_weakness`,
`ui.rs::draw_boss`, `map/layer.rs` (icons).

## Question

Show, next to the locked-on enemy (and under a boss's health bar), which damage types it takes the most or the
least from and which statuses build up fastest on it, from the game's own data.

## Where the data is

Everything is in the `NpcParam` row of the enemy (`ChrIns.npc_param_id`), read through
`SoloParamRepository::get::<NpcParam>`; fromsoftware-rs types every field, so no raw offsets are needed.

| Field | Meaning |
|---|---|
| `neutral/slash/blow/thrust/magic/fire/thunder/dark_damage_cut_rate` (`f32`) | Damage rate: above 1 the enemy takes more, below 1 less. `dark` is holy damage. Only magic, fire, thunder and dark are shown. |
| `resist_poison`, `resist_desease` (scarlet rot), `resist_blood`, `resist_curse` (death blight), `resist_freeze`, `resist_sleep`, `resist_madness` (`u16`) | Buildup needed to trigger the status: the lower, the faster. **999 is immunity.** 0 is a row without that status and is left out. |

The values are read from the params the game has loaded, so a pack that edits `NpcParam` is followed.

## Derivation

Checked on the vanilla table (7048 `NpcParam` rows, exported from Smithbox) against the log of the game for two
enemies (a soldier and a sentinel): every rate and resistance matched.

- **Nothing else changes the damage rates.** 447 `SpEffectParam` rows have a damage cut rate other than 1, and
  none of the `spEffectID0..7` of any `NpcParam` row points to one of them. The row alone is the answer.
- **The area scaling does not change the order of the statuses.** 4526 rows point to an effect that multiplies
  the status resistances (about 2.0 to 2.6, `regist*ChangeRate`); all 70 distinct effects of that kind multiply
  the six statuses by the same factor, so ranking the raw values ranks the real ones.
- **How varied the rows are** (7024 rows with health): the damage rates are mostly 0.6 to 1.4 with 2.0 for fire in
  485 rows. Counting the types that are not neutral, 21% of the rows have none, 44% one, 7% two, 17% three, 10%
  four. 97% have five statuses that are not immune (poison, rot, bleed, frost, sleep); death blight is immune in
  99% and madness in 91%.

## Showing it

- **Statuses**: the game's buildup icons (`MENU_RE_*`), fastest first from the edge the row starts at. An immune
  status goes last, in a gray copy of its icon made when the icons are uploaded (`Image::grayscale`), at 65%
  opacity. In the panel the time left of a status that has popped is written under its icon, so those effects
  do not get a cell of their own.
- **Damage types**: weapon affinity icons of the game's weapon screens, one per type that is not neutral:
  `MENU_FL_40146` magic, `MENU_FL_40148` fire, `MENU_FL_40150` lightning, `MENU_FL_40145` holy, all in sheet
  `SB_MainMenu_02` of the common menu pack. Each has green arrows pointing up for a weakness or red ones pointing
  down for a resistance, made from the game's chevron `MENU_FL_Arrow` (white, turned a quarter and tinted).
  The distinct rates on a side are ranked by their distance from 1 and the nearest gets one arrow, the next two,
  up to three. Weaknesses come first, the strongest first, then the resistances, the mildest first.
- **Panel** (any enemy without a game boss bar): the status row, then the damage row, right-aligned under the
  health and posture bars. **Boss** with `Target.BossBar`: under its posture bar, the damage on the left end of
  the bar and the statuses with their times on the right end, then its other effects. That part follows the
  screen only, not `Target.Scale`, like the game's bar.
- The icons are 0.8 times the effect icons and each takes 0.9 of an effect cell; the buildup bars' icons have
  the same size. Keys `Target.WeakDamage` and `Target.WeakEffects` turn the rows off.

## Where the icons were found

- The affinity icons are `MENU_FL_40141` to `MENU_FL_40151`, `40172` and `40173` of `SB_MainMenu_02` (74x74):
  standard, heavy, keen, quality, sacred, magic, cold, fire, lightning, a spiked orb, poison, blood. The names
  were found in the movies of the weapon screens (`02_055_detailstatus_weapon1`, `03_041_enhancedweaponselect`,
  `03_030_comparison_equip_weapon1`), which list the sprites they draw.
- Sheets without elemental sprites: `SB_Status_00` (state icons), `SB_MainMenu` (the `MENU_FL_32xxx` ones are
  body parts), `SB_Tab`, `SB_Chara_01` (class art).
- The game builds menu paths from a table of nine layout packs (`00_Solo`, `01_Common`, `02_Title`,
  `03_ChrMake`, `04_NowLoading`, `05_Dummy`, `06_Platform`, `71_MapTile`, `80_Language`; `scripts/ida/menu_layouts.py`).
  Only `01_Common` is read by the mod.
- A total-conversion pack replaces `01_common.sblytbnd.dcx` and `01_common.tpf.dcx`, and the mod reads those
  (a loose file before the archive). In the pack tried (Convergence) the four affinity rectangles are still in the
  layout but the sprites are fully transparent, since its weapon screens do not use them; the chevron and the buildup
  icons are intact. That pack draws its own affinity icons: 24 sprites of 74x74, `MENU_ItemIcon_19000` to `19023`, in a
  sheet it adds (`SB_Icon_07`); by their art 19004 is magic, 19007 fire, 19008 holy and 19009 lightning. A sprite that
  comes out blank is therefore replaced by its substitute when the pack has one (`sprites::SUBSTITUTES`), or else read
  again from the game's own archive (`sprites::restore_blank`, which logs which of the two it used).
- Other chevrons and arrows of the sheets (`MENU_Arrow`, `MENU_Change`, `MENU_SortArrow`) are gold, beige or
  framed, and do not tint well; the game has no green or red arrow sprite.

## Rejected paths

- **Damage rates from active effects.** Expected from other games, disproved above: no enemy has an effect that
  edits them.
- **Plain shapes for the damage types** (flame, star, bolt, cross drawn with the draw list): replaced by the
  game's icons once they were found.
- **Showing only the best types** (the highest rate plus any from 1.2): left out of the picture the types an
  enemy resists, which the player also wants to know. Replaced by weaknesses and resistances with arrows.
- **Hiding death blight and madness when immune**: the row differed from the one a third-party overlay shows
  for the same enemy and hid information. With the gray icons all immune statuses are shown.

## Open risks

- Only 2.7.2.0 was checked. No AOB or raw offset is involved, only the typed param fields, so a game update
  breaks this only if the `NpcParam` layout changes.
- A boss that changes `NpcParam` row between phases is read again every frame, so its rows should follow;
  not seen in game.
- The meaning of 999 as immunity is taken from another tool and the data (it is the cap of the field and the
  value in most rows); the buildup of an immune status was not tried in game. 0 is assumed to mean "no status".
- Seamless Co-op: the rows come from the local params, so they should be the same on every machine.
