//! `KeepSpiritsOnTravel`: after a fast travel (or any loading screen) the spirits that were out are
//! summoned again. The loading screen destroys the whole world and the spirit manager, so there is
//! nothing to block (`docs/persist_spirits.md`); the Ashes are summoned again through the game's own
//! request field once the world is ready.

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use eldenring::cs::{CSTaskGroupIndex, SummonBuddyManager, WorldChrMan};
use fromsoftware_shared::FromStatic;

use shared::{logger, task};

use crate::config;
use crate::multi_spirit::group_alive;

const LOADING_BLOCK: i32 = -1;
/// The world is ready this long before the first Ash is summoned again.
const SETTLE_MS: f64 = 3000.0;
/// Minimum wait between two summons: `DoSummon` takes one request at a time.
const GAP_MS: f64 = 1500.0;
/// A request the game never consumed means it refused the summon; stop waiting.
const REQUEST_TIMEOUT_MS: f64 = 10_000.0;
/// The groups may vanish a frame or two before the loading screen shows; the snapshot survives
/// that gap and is dropped only when the player really sent the spirits back.
const EMPTY_GRACE_MS: f64 = 500.0;
const CONFIG_INTERVAL_MS: f64 = 1000.0;

enum Phase {
    Tracking,
    Loading { ready_ms: f64 },
    Restoring { queue: VecDeque<i32>, wait_ms: f64, elapsed_ms: f64 },
}

struct Travel {
    phase: Phase,
    /// Trigger SpEffects of the Ashes out, in the order of the groups.
    saved: Vec<i32>,
    empty_ms: f64,
    /// Full SpEffect (upgrade level included) of each Ash, by the key `DoSummon` rounds it to.
    full_by_key: HashMap<i32, i32>,
    /// Summon goods of each Ash by the same key: the HUD takes the name and icon from it.
    goods_by_key: HashMap<i32, i32>,
    last_active: u32,
}

/// Trigger SpEffect of every Ash that has a live local group.
fn ash_keys(manager: &SummonBuddyManager) -> Vec<i32> {
    let mut keys = Vec::new();
    for group in manager.groups.iter().flat_map(|pair| pair.second.iter()) {
        if !group_alive(group) || group.is_remote {
            continue;
        }
        let key = manager
            .trigger_speffect_to_buddy_map
            .iter_chains()
            .find(|(_, head)| head.iter().any(|id| *id == group.buddy_param_id))
            .map(|(key, _)| *key);
        if let Some(key) = key
            && !keys.contains(&key)
        {
            keys.push(key);
        }
    }
    keys
}

fn has_groups(manager: &SummonBuddyManager) -> bool {
    manager.groups.iter().flat_map(|pair| pair.second.iter()).any(|group| !group.is_remote)
}

impl Travel {
    fn new() -> Travel {
        Travel {
            phase: Phase::Tracking,
            saved: Vec::new(),
            empty_ms: 0.0,
            full_by_key: HashMap::new(),
            goods_by_key: HashMap::new(),
            last_active: 0,
        }
    }

    fn reset(&mut self) {
        self.phase = Phase::Tracking;
        self.saved.clear();
        self.empty_ms = 0.0;
    }

    fn remember_active(&mut self, manager: &SummonBuddyManager) {
        let active = manager.active_summon_speffect_id;
        if active != self.last_active {
            self.last_active = active;
            if (active as i32) >= 0 {
                self.full_by_key.insert(100 * (active as i32 / 100), active as i32);
            }
        }
        if (active as i32) >= 0 && manager.active_summon_goods_id > 0 {
            self.goods_by_key.insert(100 * (active as i32 / 100), manager.active_summon_goods_id);
        }
    }

    fn step(&mut self, dt_ms: f64) {
        let in_world = shared::player::main_player_chr_ins_ptr().is_some();
        // SAFETY: called from a game task; the accessor returns `Err` while the singleton does not exist.
        let world = unsafe { WorldChrMan::instance_mut() }.ok();
        let player =
            world.as_ref().filter(|_| in_world).and_then(|w| w.main_player.as_ref()).map(|p| {
                (
                    p.current_block_id.0,
                    p.chr_ins.modules.data.hp,
                    p.chr_ins.modules.physics.position,
                )
            });
        let ready = matches!(player, Some((block, _, _)) if block != LOADING_BLOCK);

        let (Some(world), true) = (world, ready) else {
            match &mut self.phase {
                Phase::Tracking if !self.saved.is_empty() => {
                    logger::log(&format!(
                        "KeepSpiritsOnTravel: loading screen, {} Ash(es) saved",
                        self.saved.len()
                    ));
                    self.phase = Phase::Loading { ready_ms: 0.0 };
                }
                Phase::Loading { ready_ms } => *ready_ms = 0.0,
                Phase::Restoring { .. } => {
                    logger::log("KeepSpiritsOnTravel: loading again, restore stopped");
                    self.phase = Phase::Loading { ready_ms: 0.0 };
                }
                Phase::Tracking => {}
            }
            return;
        };
        let hp = player.map_or(1, |(_, hp, _)| hp);
        let position = player.map(|(_, _, position)| position);
        let manager = &mut world.summon_buddy_manager;
        self.remember_active(manager);

        match &mut self.phase {
            Phase::Tracking => {
                if hp <= 0 {
                    self.saved.clear();
                    return;
                }
                let keys = ash_keys(manager);
                if keys.is_empty() {
                    self.empty_ms += dt_ms;
                    if self.empty_ms > EMPTY_GRACE_MS {
                        self.saved.clear();
                    }
                } else {
                    self.empty_ms = 0.0;
                    self.saved = keys;
                }
            }
            Phase::Loading { ready_ms } => {
                *ready_ms += dt_ms;
                if has_groups(manager) {
                    logger::log("KeepSpiritsOnTravel: spirits survived the loading screen");
                    self.phase = Phase::Tracking;
                } else if *ready_ms >= SETTLE_MS {
                    logger::log(&format!(
                        "KeepSpiritsOnTravel: world ready, summoning {:?} again",
                        self.saved
                    ));
                    self.phase = Phase::Restoring {
                        queue: self.saved.iter().copied().collect(),
                        wait_ms: 0.0,
                        elapsed_ms: 0.0,
                    };
                }
            }
            Phase::Restoring { queue, wait_ms, elapsed_ms } => {
                *elapsed_ms += dt_ms;
                if *wait_ms > 0.0 {
                    *wait_ms -= dt_ms;
                    return;
                }
                if manager.request_summon_speffect_id != -1 {
                    if *elapsed_ms > REQUEST_TIMEOUT_MS {
                        logger::error(
                            "KeepSpiritsOnTravel: the game did not take the summon request",
                        );
                        self.phase = Phase::Tracking;
                    }
                    return;
                }
                match queue.pop_front() {
                    Some(key) => {
                        let id = self.full_by_key.get(&key).copied().unwrap_or(key);
                        // The summon item's animation sets the spawn point; without it the spirits appear at the one from before the travel.
                        if let Some(position) = position {
                            manager.spawn_origin = position;
                        }
                        let goods = self.goods_by_key.get(&key).copied().unwrap_or(0);
                        if goods > 0 {
                            manager.requested_summon_goods_id = goods;
                            manager.active_summon_goods_id = goods;
                        }
                        // The item use sets the active stone; left at 0, the next press of the Ash reads "moved to another stone area" and summons instead of sending back.
                        if manager.buddy_stone_entity_id != 0 {
                            manager.active_summmon_buddy_stone_entity_id =
                                manager.buddy_stone_entity_id;
                        }
                        manager.request_summon_speffect_id = id;
                        logger::log(&format!(
                            "KeepSpiritsOnTravel: requested SpEffect {id}, goods {goods}"
                        ));
                        *wait_ms = GAP_MS;
                    }
                    None => {
                        logger::log(&format!(
                            "KeepSpiritsOnTravel: done, {} Ash(es) out",
                            ash_keys(manager).len()
                        ));
                        self.phase = Phase::Tracking;
                    }
                }
            }
        }
    }
}

pub fn run() {
    let cs_task = task::wait_for_cs_task();
    let mut travel = Travel::new();
    let mut enabled = false;
    let mut config_ms = CONFIG_INTERVAL_MS;
    let _handle = task::run_recurring_safe(
        cs_task,
        "KeepSpiritsOnTravel",
        CSTaskGroupIndex::FrameBegin,
        move |data: &eldenring::fd4::FD4TaskData| {
            let dt_ms = (data.delta_time.time as f64) * 1000.0;
            config_ms += dt_ms;
            if config_ms >= CONFIG_INTERVAL_MS {
                config_ms = 0.0;
                let on = config::get().spirit.keep_spirits_on_travel;
                if on != enabled {
                    enabled = on;
                    logger::log(&format!("KeepSpiritsOnTravel={on}."));
                    travel.reset();
                }
            }
            if enabled {
                travel.step(dt_ms);
            }
        },
    );
    loop {
        std::thread::sleep(Duration::from_secs(60));
    }
}
