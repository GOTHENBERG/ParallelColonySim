use std::time::{Duration, Instant};

use bevy_ecs::prelude::*;
use bevy_ecs::schedule::MultiThreadedExecutor;
use bevy_tasks::{ComputeTaskPool, TaskPoolBuilder};

pub const CHUNK_EDGE: usize = 32;
pub const DEFAULT_COLONISTS: usize = 5;

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum ItemKind {
    Berry,
    Meal,
    Wood,
    Stone,
    Medicine,
    Metal,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum JobKind {
    GatherFood,
    Cook,
    HarvestWood,
    MineStone,
    Haul,
    Build,
    TendCrops,
    TreatInjuries,
    Rest,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PlayerCommand {
    RecruitColonist {
        at_x: u16,
        at_y: u16,
    },
    SetJobPriority {
        colonist_id: u32,
        job: JobKind,
        priority: u8,
    },
    AssignJob {
        colonist_id: u32,
        job: JobKind,
        target_id: u32,
    },
    CancelJob {
        colonist_id: u32,
    },
}

#[derive(Clone, Copy, Debug)]
pub struct Scenario {
    pub name: &'static str,
    pub map_edge: usize,
    pub colonists: usize,
    pub ticks: u64,
}

impl Scenario {
    pub const fn default_colony() -> Self {
        Self {
            name: "default-5",
            map_edge: 128,
            colonists: DEFAULT_COLONISTS,
            ticks: 500,
        }
    }

    pub const fn medium() -> Self {
        Self {
            name: "medium-50",
            map_edge: 256,
            colonists: 50,
            ticks: 500,
        }
    }

    pub const fn stress() -> Self {
        Self {
            name: "stress-500",
            map_edge: 512,
            colonists: 500,
            ticks: 150,
        }
    }

    pub fn chunk_count(self) -> usize {
        let chunks_per_edge = self.map_edge.div_ceil(CHUNK_EDGE);
        chunks_per_edge * chunks_per_edge
    }

    pub fn tile_count(self) -> usize {
        self.chunk_count() * CHUNK_EDGE * CHUNK_EDGE
    }
}

#[derive(Resource, Default)]
struct SimTick(u64);

#[derive(Component)]
struct StableId(u32);

#[derive(Component)]
struct Needs {
    hunger: u16,
    rest: u16,
    comfort: u16,
}

#[derive(Component)]
struct WorkPriorities([u8; 9]);

#[derive(Component)]
struct CurrentJob {
    kind: JobKind,
    target_id: u32,
    work_left: u16,
}

#[derive(Component)]
struct Position {
    x: u16,
    y: u16,
}

#[derive(Component)]
struct NavigationState {
    path_version: u32,
    remaining_steps: u16,
}

#[derive(Clone, Copy)]
struct Tile {
    moisture: u16,
    fertility: u16,
    temperature: i16,
    vegetation: u16,
    stone: u16,
    wood: u16,
}

#[derive(Component)]
struct MapChunk {
    id: u32,
    side: usize,
    current: Vec<Tile>,
    next: Vec<Tile>,
}

#[derive(Resource)]
struct WorldInfo {
    width: usize,
    height: usize,
    seed: u64,
    checksum: u64,
}

#[derive(SystemSet, Clone, Debug, Eq, Hash, PartialEq)]
enum TickPhase {
    IndependentSimulation,
    Decisions,
    Commit,
}

#[derive(Debug)]
pub struct ProfileResult {
    pub elapsed: Duration,
    pub checksum: u64,
    pub active_threads: usize,
    pub scenario: Scenario,
}

pub fn available_worker_count() -> usize {
    std::thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(1)
        .max(1)
}

pub fn run_profile(scenario: Scenario, workers: usize, warmup_ticks: u64) -> ProfileResult {
    let workers = workers.max(1);
    ComputeTaskPool::get_or_init(|| {
        TaskPoolBuilder::new()
            .num_threads(workers)
            .thread_name("parallel-colony-compute".to_owned())
            .build()
    });

    let mut world = World::new();
    let width = scenario.map_edge;
    let height = scenario.map_edge;
    world.insert_resource(SimTick::default());
    world.insert_resource(WorldInfo {
        width,
        height,
        seed: 0x5043_5300_0000_0001,
        checksum: 0,
    });
    spawn_chunks(&mut world, width, height);
    spawn_colonists(&mut world, scenario.colonists, width, height);

    let mut schedule = Schedule::default();
    schedule.set_executor(MultiThreadedExecutor::new());
    schedule.configure_sets(
        (
            TickPhase::IndependentSimulation,
            TickPhase::Decisions,
            TickPhase::Commit,
        )
            .chain(),
    );
    schedule
        .add_systems((advance_environment, update_needs).in_set(TickPhase::IndependentSimulation));
    schedule.add_systems((choose_jobs, update_navigation).in_set(TickPhase::Decisions));
    schedule.add_systems(commit_tick.in_set(TickPhase::Commit));

    for _ in 0..warmup_ticks {
        schedule.run(&mut world);
    }

    let started = Instant::now();
    for _ in 0..scenario.ticks {
        schedule.run(&mut world);
    }
    let elapsed = started.elapsed();

    let (seed, width, height) = {
        let info = world.resource::<WorldInfo>();
        (info.seed, info.width, info.height)
    };
    let mut checksum = seed ^ width as u64 ^ ((height as u64) << 32);
    for chunk in world.query::<&MapChunk>().iter(&world) {
        checksum = checksum.rotate_left(7) ^ chunk_checksum(chunk);
    }
    for (stable_id, needs) in world.query::<(&StableId, &Needs)>().iter(&world) {
        checksum ^= (stable_id.0 as u64) << 32
            ^ needs.hunger as u64
            ^ ((needs.rest as u64) << 16)
            ^ ((needs.comfort as u64) << 48);
    }
    for (stable_id, job) in world.query::<(&StableId, &CurrentJob)>().iter(&world) {
        checksum ^= stable_id.0 as u64 ^ job.target_id as u64 ^ job.work_left as u64;
    }
    world.resource_mut::<WorldInfo>().checksum = checksum;
    let active_threads = ComputeTaskPool::get().thread_num();
    ProfileResult {
        elapsed,
        checksum,
        active_threads,
        scenario,
    }
}

fn spawn_chunks(world: &mut World, width: usize, height: usize) {
    let chunks_x = width.div_ceil(CHUNK_EDGE);
    let chunks_y = height.div_ceil(CHUNK_EDGE);
    for cy in 0..chunks_y {
        for cx in 0..chunks_x {
            let id = (cy * chunks_x + cx) as u32;
            let mut current = Vec::with_capacity(CHUNK_EDGE * CHUNK_EDGE);
            for local_y in 0..CHUNK_EDGE {
                for local_x in 0..CHUNK_EDGE {
                    let x = cx * CHUNK_EDGE + local_x;
                    let y = cy * CHUNK_EDGE + local_y;
                    current.push(seed_tile(x, y));
                }
            }
            let next = current.clone();
            world.spawn(MapChunk {
                id,
                side: CHUNK_EDGE,
                current,
                next,
            });
        }
    }
}

fn seed_tile(x: usize, y: usize) -> Tile {
    let mixed = (x as u64).wrapping_mul(0x9E37_79B1)
        ^ (y as u64).wrapping_mul(0x85EB_CA77)
        ^ 0x5043_5300_0000_0001;
    let value = mixed ^ (mixed >> 29);
    Tile {
        moisture: (value & 1023) as u16,
        fertility: ((value >> 10) & 1023) as u16,
        temperature: ((value >> 20) as i16 % 500) - 150,
        vegetation: ((value >> 30) & 1023) as u16,
        stone: ((value >> 40) & 1023) as u16,
        wood: ((value >> 50) & 1023) as u16,
    }
}

fn spawn_colonists(world: &mut World, count: usize, width: usize, height: usize) {
    for index in 0..count {
        let id = index as u32 + 1;
        let x = ((index * 7 + 3) % width) as u16;
        let y = ((index * 11 + 5) % height) as u16;
        let job = match index % 7 {
            0 => JobKind::GatherFood,
            1 => JobKind::HarvestWood,
            2 => JobKind::MineStone,
            3 => JobKind::Haul,
            4 => JobKind::TendCrops,
            5 => JobKind::Cook,
            _ => JobKind::Build,
        };
        world.spawn((
            StableId(id),
            Needs {
                hunger: 100 + (id as u16 * 13 % 800),
                rest: 200 + (id as u16 * 17 % 700),
                comfort: 300 + (id as u16 * 19 % 600),
            },
            WorkPriorities([5, 4, 3, 4, 3, 3, 2, 1, 2]),
            CurrentJob {
                kind: job,
                target_id: id * 3,
                work_left: 100,
            },
            Position { x, y },
            NavigationState {
                path_version: 0,
                remaining_steps: 20 + (id as u16 % 50),
            },
        ));
    }
}

fn advance_environment(mut chunks: Query<&mut MapChunk>, tick: Res<SimTick>) {
    let season_bias = ((tick.0 / 240) % 40) as i16 - 20;
    chunks.par_iter_mut().for_each(|mut chunk| {
        let side = chunk.side;
        for y in 0..side {
            for x in 0..side {
                let i = y * side + x;
                let old = chunk.current[i];
                let left = chunk.current[y * side + x.saturating_sub(1)];
                let right = chunk.current[y * side + (x + 1).min(side - 1)];
                let up = chunk.current[y.saturating_sub(1) * side + x];
                let down = chunk.current[(y + 1).min(side - 1) * side + x];
                let neighboring_moisture = (left.moisture as u32
                    + right.moisture as u32
                    + up.moisture as u32
                    + down.moisture as u32)
                    / 4;
                let evaporation = (old.temperature.max(0) as u16 / 90).min(7);
                let moisture = ((old.moisture as u32 * 7 + neighboring_moisture) / 8)
                    .saturating_sub(evaporation as u32)
                    .min(1023) as u16;
                let temperature = (old.temperature + season_bias).clamp(-250, 450);
                let growth = if (-50..=320).contains(&temperature) && moisture > 180 {
                    1 + old.fertility / 512
                } else {
                    0
                };
                let vegetation = (old.vegetation.saturating_add(growth)).min(1023);
                let wood = if vegetation > 700 && tick.0.is_multiple_of(31) {
                    old.wood.saturating_add(1).min(1023)
                } else {
                    old.wood
                };
                chunk.next[i] = Tile {
                    moisture,
                    fertility: old
                        .fertility
                        .saturating_sub(u16::from(growth > 0 && tick.0.is_multiple_of(120))),
                    temperature,
                    vegetation,
                    stone: old.stone,
                    wood,
                };
            }
        }
        let chunk = &mut *chunk;
        std::mem::swap(&mut chunk.current, &mut chunk.next);
    });
}

fn update_needs(mut query: Query<(&StableId, &mut Needs)>, tick: Res<SimTick>) {
    query.par_iter_mut().for_each(|(id, mut needs)| {
        needs.hunger = needs.hunger.saturating_add(1 + (id.0 % 3) as u16).min(1000);
        if tick.0.is_multiple_of(4) {
            needs.rest = needs.rest.saturating_sub(1);
        } else {
            needs.rest = needs.rest.saturating_add(1).min(1000);
        }
        needs.comfort = (needs.comfort + (id.0 as u16 % 5)).min(1000);
    });
}

fn choose_jobs(
    mut query: Query<(&StableId, &Needs, &WorkPriorities, &mut CurrentJob)>,
    tick: Res<SimTick>,
) {
    query
        .par_iter_mut()
        .for_each(|(id, needs, priorities, mut job)| {
            if needs.hunger > 800 {
                job.kind = JobKind::GatherFood;
            } else if needs.rest < 180 {
                job.kind = JobKind::Rest;
            } else if tick.0.is_multiple_of(120) {
                let offset = (id.0 as usize + tick.0 as usize / 120) % priorities.0.len();
                let selected = (0..priorities.0.len())
                    .max_by_key(|candidate| priorities.0[(candidate + offset) % priorities.0.len()])
                    .unwrap_or(0);
                job.kind = job_from_index(selected);
            }
            job.target_id = id.0.wrapping_mul(17).wrapping_add(tick.0 as u32 % 97);
            job.work_left = job.work_left.saturating_sub(1);
            if job.work_left == 0 {
                job.work_left = 80 + (id.0 as u16 % 60);
            }
        });
}

fn update_navigation(
    mut query: Query<(&StableId, &Position, &CurrentJob, &mut NavigationState)>,
    info: Res<WorldInfo>,
    tick: Res<SimTick>,
) {
    query
        .par_iter_mut()
        .for_each(|(id, position, job, mut nav)| {
            let target_x = (job.target_id as usize * 13 % info.width) as u16;
            let target_y = (job.target_id as usize * 29 % info.height) as u16;
            nav.remaining_steps = position.x.abs_diff(target_x) + position.y.abs_diff(target_y);
            nav.path_version = (info.seed as u32).wrapping_add(tick.0 as u32 / 30);
            nav.remaining_steps = nav.remaining_steps.wrapping_add((id.0 % 3) as u16);
        });
}

fn commit_tick(mut tick: ResMut<SimTick>) {
    tick.0 = tick.0.wrapping_add(1);
}

fn job_from_index(index: usize) -> JobKind {
    match index {
        0 => JobKind::GatherFood,
        1 => JobKind::Cook,
        2 => JobKind::HarvestWood,
        3 => JobKind::MineStone,
        4 => JobKind::Haul,
        5 => JobKind::Build,
        6 => JobKind::TendCrops,
        7 => JobKind::TreatInjuries,
        _ => JobKind::Rest,
    }
}

fn chunk_checksum(chunk: &MapChunk) -> u64 {
    let mut checksum = chunk.id as u64;
    for (index, tile) in chunk.current.iter().enumerate().step_by(17) {
        checksum = checksum.rotate_left(5)
            ^ tile.moisture as u64
            ^ ((tile.fertility as u64) << 10)
            ^ ((tile.temperature as u16 as u64) << 20)
            ^ ((tile.vegetation as u64) << 36)
            ^ ((tile.wood as u64) << 48)
            ^ index as u64;
    }
    checksum
}
