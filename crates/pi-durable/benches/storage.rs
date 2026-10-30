//! Storage benchmarks over the memory, SQLite and JSONL backends: the shared read and write scenarios of
//! `pi_durable::testing` (validated against their expected values first), and reopening a persistent store followed
//! by its first exact read.
//!
//!   cargo bench -p pi-durable --bench storage          (PI_DURABLE_BENCH_QUICK=1 for few iterations)
mod support;
use pi_durable::storage::sqlite::{NativeSqliteStorageOptions, open_native_sqlite_storage};
use pi_durable::testing::*;
use pi_durable::*;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use support::*;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Backend {
    Memory,
    Sqlite,
    Jsonl,
}
impl Backend {
    const ALL: [Backend; 3] = [Backend::Memory, Backend::Sqlite, Backend::Jsonl];
    fn name(self) -> &'static str {
        match self {
            Backend::Memory => "memory",
            Backend::Sqlite => "sqlite",
            Backend::Jsonl => "jsonl",
        }
    }
}
struct Options {
    iterations: usize,
    warmup: usize,
    min_time: Duration,
}
fn read_options() -> Options {
    if quick() {
        Options {
            iterations: 3,
            warmup: 1,
            min_time: Duration::ZERO,
        }
    } else {
        Options {
            iterations: 10,
            warmup: 3,
            min_time: Duration::from_millis(300),
        }
    }
}
fn write_options() -> Options {
    if quick() {
        Options {
            iterations: 3,
            warmup: 1,
            min_time: Duration::ZERO,
        }
    } else {
        Options {
            iterations: 20,
            warmup: 5,
            min_time: Duration::ZERO,
        }
    }
}

async fn open(backend: Backend, directory: &Path) -> Result<Arc<dyn Storage>> {
    let context = ctx();
    Ok(match backend {
        Backend::Memory => Arc::new(MemoryStorage::new()),
        Backend::Sqlite => Arc::new(
            open_native_sqlite_storage(
                directory.join("storage.sqlite"),
                NativeSqliteStorageOptions::default(),
            )
            .await?,
        ),
        Backend::Jsonl => Arc::new(
            JsonlStorage::open_native(directory.join("storage"), &context, Default::default())
                .await?,
        ),
    })
}
fn path_of(backend: Backend, directory: &Path) -> PathBuf {
    match backend {
        Backend::Sqlite => directory.join("storage.sqlite"),
        _ => directory.join("storage"),
    }
}
fn copy_store(backend: Backend, from: &Path, to: &Path) -> std::io::Result<()> {
    if backend == Backend::Sqlite {
        std::fs::copy(from, to).map(|_| ())
    } else {
        std::fs::create_dir_all(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            std::fs::copy(entry.path(), to.join(entry.file_name()))?;
        }
        Ok(())
    }
}
fn summary(samples: &mut [Duration]) -> String {
    samples.sort();
    let mean = samples.iter().sum::<Duration>() / samples.len() as u32;
    format!(
        "mean {:>9.3} ms  min {:>9.3} ms  p50 {:>9.3} ms",
        mean.as_secs_f64() * 1000.0,
        samples[0].as_secs_f64() * 1000.0,
        samples[samples.len() / 2].as_secs_f64() * 1000.0
    )
}

#[tokio::main]
async fn main() -> Result<()> {
    let context = ctx();
    let scale = TIMING_SCALE;
    let mut keep = Vec::new();
    println!(
        "== read scenarios ({} entries, {} tasks, {} documents)",
        scale.entry_count, scale.task_count, scale.document_count
    );
    let mut stores = Vec::new();
    for backend in Backend::ALL {
        let directory = tempfile::tempdir().map_err(bench_err)?;
        let storage = open(backend, directory.path()).await?;
        let dataset = Arc::new(seed_storage_benchmark(&*storage, &scale).await?);
        stores.push((backend, storage, dataset));
        keep.push(directory);
    }
    let options = read_options();
    for scenario in storage_read_benchmarks() {
        for (backend, storage, dataset) in &stores {
            let got = scenario.run(storage.clone(), dataset.clone()).await?;
            if got != scenario.expected(dataset) {
                return Err(bench_err(format!(
                    "{} on {}: got {got}",
                    scenario.name,
                    backend.name()
                )));
            }
            for _ in 0..options.warmup {
                scenario.run(storage.clone(), dataset.clone()).await?;
            }
            let mut samples = Vec::new();
            let begun = Instant::now();
            while samples.len() < options.iterations || begun.elapsed() < options.min_time {
                let start = Instant::now();
                scenario.run(storage.clone(), dataset.clone()).await?;
                samples.push(start.elapsed());
            }
            println!(
                "{:<40} {:<7} {}",
                scenario.name,
                backend.name(),
                summary(&mut samples)
            );
        }
    }

    println!("== write scenarios");
    let options = write_options();
    for scenario in storage_write_benchmarks() {
        for backend in Backend::ALL {
            let mut pool = Vec::new();
            for _ in 0..options.iterations + options.warmup + 1 {
                let directory = tempfile::tempdir().map_err(bench_err)?;
                let storage = open(backend, directory.path()).await?;
                seed_storage_write_benchmark(&*storage).await?;
                pool.push((storage, directory));
            }
            let (validation, _dir) = pool.pop().expect("validation fixture");
            let got = scenario.run(validation).await?;
            if got != scenario.expected {
                return Err(bench_err(format!(
                    "{} on {}: got {got}",
                    scenario.name,
                    backend.name()
                )));
            }
            let mut samples = Vec::new();
            for (index, (storage, _directory)) in pool.iter().enumerate() {
                let start = Instant::now();
                scenario.run(storage.clone()).await?;
                if index >= options.warmup {
                    samples.push(start.elapsed());
                }
            }
            println!(
                "{:<40} {:<7} {}",
                scenario.name,
                backend.name(),
                summary(&mut samples)
            );
            for (storage, _) in &pool {
                storage.close(&context).await?;
            }
        }
    }

    println!("== reopen and first exact read");
    let options = Options {
        iterations: if quick() { 3 } else { 20 },
        warmup: if quick() { 1 } else { 3 },
        min_time: Duration::ZERO,
    };
    for backend in [Backend::Sqlite, Backend::Jsonl] {
        let seed_dir = tempfile::tempdir().map_err(bench_err)?;
        let seed = open(backend, seed_dir.path()).await?;
        let dataset = seed_storage_benchmark(&*seed, &scale).await?;
        seed.close(&context).await?;
        let seed_path = path_of(backend, seed_dir.path());
        let mut samples = Vec::new();
        let mut reopened = Vec::new();
        for index in 0..options.iterations + options.warmup {
            let sample = tempfile::tempdir().map_err(bench_err)?;
            copy_store(backend, &seed_path, &path_of(backend, sample.path())).map_err(bench_err)?;
            let start = Instant::now();
            let storage = open(backend, sample.path()).await?;
            let id = storage
                .entry(dataset.first_entry_id, &context)
                .await?
                .map(|stored| stored.entry.id);
            let elapsed = start.elapsed();
            if id != Some(dataset.first_entry_id) {
                return Err(bench_err("the reopened store lost its first entry"));
            }
            if index >= options.warmup {
                samples.push(elapsed);
            }
            reopened.push((storage, sample));
        }
        println!(
            "{:<40} {:<7} {}",
            "reopen and first exact read",
            backend.name(),
            summary(&mut samples)
        );
        for (storage, _) in reopened {
            storage.close(&context).await?;
        }
    }
    for (_, storage, _) in stores {
        storage.close(&context).await?;
    }
    Ok(())
}
