use std::process::ExitCode;

use sim_core::{Scenario, available_worker_count, run_profile};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<(), String> {
    let mut scenario_name = "default".to_owned();
    let mut workers = available_worker_count();
    let mut warmup = 30_u64;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--scenario" => {
                scenario_name = args.next().ok_or("--scenario needs a value")?;
            }
            "--threads" => {
                let value = args.next().ok_or("--threads needs a value")?;
                workers = if value == "auto" {
                    available_worker_count()
                } else {
                    value
                        .parse::<usize>()
                        .map_err(|_| "--threads must be 'auto' or a positive integer")?
                };
            }
            "--warmup" => {
                warmup = args
                    .next()
                    .ok_or("--warmup needs a value")?
                    .parse()
                    .map_err(|_| "--warmup must be a non-negative integer")?;
            }
            "--help" | "-h" => {
                print_help();
                return Ok(());
            }
            unknown => return Err(format!("unknown argument: {unknown}; use --help")),
        }
    }
    if workers == 0 {
        return Err("--threads must be greater than zero".to_owned());
    }

    let scenario = match scenario_name.as_str() {
        "default" => Scenario::default_colony(),
        "medium" => Scenario::medium(),
        "stress" => Scenario::stress(),
        other => return Err(format!("unknown scenario: {other}")),
    };
    let result = run_profile(scenario, workers, warmup);
    println!("scenario,width,colonists,chunks,tiles,ticks,workers,elapsed_ms,ns_per_tick,checksum");
    println!(
        "{},{},{},{},{},{},{},{:.3},{:.0},{:016x}",
        result.scenario.name,
        result.scenario.map_edge,
        result.scenario.colonists,
        result.scenario.chunk_count(),
        result.scenario.tile_count(),
        result.scenario.ticks,
        result.active_threads,
        result.elapsed.as_secs_f64() * 1000.0,
        result.elapsed.as_nanos() as f64 / result.scenario.ticks as f64,
        result.checksum,
    );
    Ok(())
}

fn print_help() {
    println!(
        "ParallelColonySim baseline profiler\n\
         Usage: sim-profile [--scenario default|medium|stress] [--threads auto|N] [--warmup N]\n\
         default: 5 colonists, 128x128 map, 500 measured ticks\n\
         medium: 50 colonists, 256x256 map, 500 measured ticks\n\
         stress: 500 colonists, 512x512 map, 150 measured ticks"
    );
}
