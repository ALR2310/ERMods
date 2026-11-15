//! What the locked-on enemy is weak to, from its `NpcParam` row (`docs/weakness.md`): how it takes
//! each damage type, and the statuses that build up fastest on it.

use eldenring::cs::{NpcParam, SoloParamRepository};
use fromsoftware_shared::FromStatic;

use crate::settings;

/// The damage types shown.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Damage {
    Magic,
    Fire,
    Lightning,
    Holy,
}

const DAMAGE: [Damage; 4] = [Damage::Magic, Damage::Fire, Damage::Lightning, Damage::Holy];

/// How far a rate has to be from 1 to count as a weakness or a resistance.
const NEUTRAL_BAND: f32 = 0.001;

/// A status resistance this high is immunity; 0 is a row that has no status at all.
const IMMUNE: u16 = 999;

#[derive(Clone, Debug)]
pub struct Weakness {
    /// `(type, level)`: above 0 it is a weakness, below 0 a resistance, and the further from 0 the
    /// further the rate is from 1 among the types. Weaknesses first, strongest first, then the
    /// resistances, the mildest first; neutral types are left out.
    pub damage: Vec<(Damage, i8)>,
    /// `(index into target::BUILDUP, immune)`: the fastest to build up first, the immune last.
    pub statuses: Vec<(usize, bool)>,
}

/// The weakness of `NpcParam` row `npc_param_id`; `None` when both rows of icons are off or the row
/// cannot be read.
pub fn read(npc_param_id: i32) -> Option<Weakness> {
    if !settings::weak_damage() && !settings::weak_effects() {
        return None;
    }
    // SAFETY: the accessor returns `Err` while the singleton does not exist.
    let repo = unsafe { SoloParamRepository::instance() }.ok()?;
    let row = repo.get::<NpcParam>(u32::try_from(npc_param_id).ok()?)?;
    Some(Weakness {
        damage: pick_damage([
            row.magic_damage_cut_rate(),
            row.fire_damage_cut_rate(),
            row.thunder_damage_cut_rate(),
            row.dark_damage_cut_rate(),
        ]),
        statuses: rank_statuses([
            row.resist_poison(),
            row.resist_desease(),
            row.resist_blood(),
            row.resist_curse(),
            row.resist_freeze(),
            row.resist_sleep(),
            row.resist_madness(),
        ]),
    })
}

/// The weaknesses and resistances among `rates`, each with its level: the distinct distances from 1
/// on a side are ranked, the nearest being level 1.
fn pick_damage(rates: [f32; 4]) -> Vec<(Damage, i8)> {
    // How far each type is from 1 on the weak side (`sign` 1) or the resisted side (`sign` -1).
    let away = |sign: f32| -> Vec<(Damage, f32)> {
        DAMAGE
            .into_iter()
            .zip(rates)
            .map(|(damage, rate)| (damage, (rate - 1.0) * sign))
            .filter(|&(_, away)| away > NEUTRAL_BAND)
            .collect()
    };
    let level = |side: &[(Damage, f32)], away: f32| -> i8 {
        let mut nearer: Vec<f32> = side
            .iter()
            .map(|&(_, other)| other)
            .filter(|&other| other < away - NEUTRAL_BAND)
            .collect();
        nearer.sort_by(f32::total_cmp);
        nearer.dedup_by(|a, b| (*a - *b).abs() < NEUTRAL_BAND);
        nearer.len() as i8 + 1
    };
    let (mut weak, mut resist) = (away(1.0), away(-1.0));
    weak.sort_by(|a, b| b.1.total_cmp(&a.1));
    resist.sort_by(|a, b| a.1.total_cmp(&b.1));
    let weak_levels = weak.iter().map(|&(damage, away)| (damage, level(&weak, away)));
    let resist_levels = resist.iter().map(|&(damage, away)| (damage, -level(&resist, away)));
    weak_levels.chain(resist_levels).collect()
}

/// `resists` in the order of `target::BUILDUP`.
fn rank_statuses(resists: [u16; 7]) -> Vec<(usize, bool)> {
    let mut open: Vec<(usize, u16)> = resists
        .into_iter()
        .enumerate()
        .filter(|&(_, resist)| resist != 0 && resist < IMMUNE)
        .collect();
    open.sort_by_key(|&(kind, resist)| (resist, kind));
    let immune = resists.into_iter().enumerate().filter(|&(_, resist)| resist >= IMMUNE);
    open.into_iter()
        .map(|(kind, _)| (kind, false))
        .chain(immune.map(|(kind, _)| (kind, true)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_enemy_weak_to_one_type_marks_it_and_leaves_neutral_ones_out() {
        // A soldier: lightning 1.2, the rest 1.0.
        assert_eq!(pick_damage([1.0, 1.0, 1.2, 1.0]), vec![(Damage::Lightning, 1)]);
    }

    #[test]
    fn an_enemy_that_resists_everything_marks_every_type() {
        // A boss: magic and holy 0.6, fire and lightning 0.8.
        assert_eq!(
            pick_damage([0.6, 0.8, 0.8, 0.6]),
            vec![
                (Damage::Fire, -1),
                (Damage::Lightning, -1),
                (Damage::Magic, -2),
                (Damage::Holy, -2)
            ]
        );
    }

    #[test]
    fn weaknesses_come_first_and_the_stronger_the_higher_the_level() {
        assert_eq!(
            pick_damage([1.4, 2.0, 1.1, 0.8]),
            vec![(Damage::Fire, 3), (Damage::Magic, 2), (Damage::Lightning, 1), (Damage::Holy, -1)]
        );
    }

    #[test]
    fn equal_types_show_nothing() {
        assert!(pick_damage([1.0; 4]).is_empty());
    }

    #[test]
    fn statuses_go_from_the_easiest_to_the_immune_ones() {
        // A soldier: poison and rot 84, bleed, frost and sleep 112, blight and madness immune.
        assert_eq!(
            rank_statuses([84, 84, 112, 999, 112, 112, 999]),
            vec![(0, false), (1, false), (2, false), (4, false), (5, false), (3, true), (6, true)]
        );
    }

    #[test]
    fn an_immune_status_is_marked_and_comes_last() {
        assert_eq!(
            rank_statuses([999, 154, 84, 999, 252, 154, 999]),
            vec![(2, false), (1, false), (5, false), (4, false), (0, true), (3, true), (6, true)]
        );
    }

    #[test]
    fn a_status_the_enemy_can_suffer_comes_before_the_immune_ones() {
        assert_eq!(
            rank_statuses([999, 999, 999, 100, 999, 999, 50])[..2],
            [(6, false), (3, false)]
        );
    }

    #[test]
    fn a_status_with_no_resistance_value_is_left_out() {
        assert_eq!(rank_statuses([0, 84, 84, 999, 84, 84, 0]).len(), 5);
    }
}
