// extern crate chrono;
extern crate clap;
extern crate crossbeam_ebr;
extern crate crossbeam_pebr;
extern crate csv;
extern crate jemalloc_ctl;
extern crate pebr_benchmark;

use clap::{arg_enum, value_t, App, Arg, ArgMatches};
use crossbeam_utils::thread::scope;
use csv::Writer;
use rand::distributions::{Uniform, WeightedIndex};
use rand::prelude::*;
use std::cmp::{max, min};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::mem::ManuallyDrop;
use std::sync::{mpsc, Arc, Barrier};
use std::time::{Duration, Instant};
use typenum::{Unsigned, U1, U4};

use pebr_benchmark::ebr;
use pebr_benchmark::pebr;

arg_enum! {
    #[derive(PartialEq, Debug)]
    pub enum DS {
        List,
        HashMap,
        NMTree,
        BonsaiTree
    }
}

arg_enum! {
    #[derive(PartialEq, Debug)]
    pub enum MM {
        NR,
        EBR,
        PEBR,
    }
}

pub enum OpsPerCs {
    One,
    Four,
}

impl fmt::Display for OpsPerCs {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OpsPerCs::One => write!(f, "1"),
            OpsPerCs::Four => write!(f, "4"),
        }
    }
}

#[derive(PartialEq, Debug)]
pub enum Op {
    Get,
    Insert,
    Remove,
}

impl Op {
    const OPS: [Op; 3] = [Op::Get, Op::Insert, Op::Remove];
}

struct Config {
    ds: DS,
    mm: MM,
    threads: usize,

    aux_thread: usize,
    aux_thread_period: Duration,
    non_coop: usize,
    non_coop_period: Duration,
    sampling: bool,
    sampling_period: Duration,

    get_rate: usize,
    op_dist: WeightedIndex<i32>,
    key_dist: Uniform<usize>,
    prefill: usize,
    interval: u64,
    duration: Duration,
    ops_per_cs: OpsPerCs,

    epoch_mib: jemalloc_ctl::epoch_mib,
    allocated_mib: jemalloc_ctl::stats::allocated_mib,
}

fn main() {
    let matches = App::new("pebr_benchmark")
        .arg(
            Arg::with_name("data structure")
                .short("d")
                .value_name("DS")
                .possible_values(&DS::variants())
                .required(true)
                .case_insensitive(true)
                .help("Data structure(s)"),
        )
        .arg(
            Arg::with_name("memory manager")
                .short("m")
                .value_name("MM")
                .possible_values(&MM::variants())
                .required(true)
                .case_insensitive(true)
                .help("Memeory manager(s)"),
        )
        .arg(
            Arg::with_name("threads")
                .short("t")
                .value_name("THREADS")
                .takes_value(true)
                .required(true)
                .help("Numbers of threads to run."),
        )
        .arg(
            Arg::with_name("non-coop")
                .short("n")
                .takes_value(false)
                .multiple(true)
                .help(
                    "The degree of non-cooperation. \
                     -n for 10ms, -nn for inf",
                ),
        )
        .arg(
            Arg::with_name("get rate")
                .short("g")
                .takes_value(false)
                .multiple(true)
                .help(
                    "The proportion of `get`(read) operations. \
                     none: 0%, -g: 50%, -gg: 90%",
                ),
        )
        .arg(
            Arg::with_name("range")
                .short("r")
                .value_name("RANGE")
                .takes_value(true)
                .help("Key range: [0..RANGE]")
                .default_value("100000"),
        )
        .arg(
            Arg::with_name("prefill")
                .short("p")
                .value_name("PREFILL")
                .takes_value(true)
                .help("The number of pre-inserted elements before starting")
                .default_value("50000"),
        )
        .arg(
            Arg::with_name("interval")
                .short("i")
                .value_name("INTERVAL")
                .takes_value(true)
                .help("Time interval in seconds to run the benchmark")
                .default_value("10"),
        )
        .arg(
            Arg::with_name("sampling period")
                .short("s")
                .value_name("MEM_SAMPLING_PERIOD")
                .takes_value(true)
                .help("The period to query jemalloc stats.allocated (ms). 0 for no sampling")
                .default_value("0"),
        )
        .arg(
            Arg::with_name("ops per cs")
                .short("c")
                .value_name("OPS_PER_CS")
                .takes_value(true)
                .possible_values(&["1", "4"])
                .help("Operations per each critical section")
                .default_value("1"),
        )
        .arg(
            Arg::with_name("output")
                .short("o")
                .value_name("OUTPUT")
                .takes_value(true)
                .help(
                    "Output CSV filename. \
                     Appends the data if the file already exists.\n\
                     [default: <DS>_results.csv]",
                ),
        )
        .get_matches();

    let (config, mut output) = setup(matches);
    match config.ops_per_cs {
        OpsPerCs::One => bench::<U1>(&config, &mut output),
        OpsPerCs::Four => bench::<U4>(&config, &mut output),
    }
}

fn setup(m: ArgMatches) -> (Config, Writer<File>) {
    let ds = value_t!(m, "data structure", DS).unwrap();
    let mm = value_t!(m, "memory manager", MM).unwrap();
    let threads = value_t!(m, "threads", usize).unwrap();
    let non_coop = min(2, m.occurrences_of("non-coop")) as usize;
    let get_rate = min(2, m.occurrences_of("get rate")) as usize;
    let range = value_t!(m, "range", usize).unwrap();
    let key_dist = Uniform::from(0..range);
    let prefill = value_t!(m, "prefill", usize).unwrap();
    let interval = value_t!(m, "interval", u64).unwrap();
    let sampling_period = value_t!(m, "sampling period", u64).unwrap();
    let sampling = sampling_period > 0;
    let ops_per_cs = match value_t!(m, "ops per cs", usize).unwrap() {
        1 => OpsPerCs::One,
        4 => OpsPerCs::Four,
        _ => panic!("ops_per_cs should be one or four"),
    };
    let duration = Duration::from_secs(interval);

    let op_weights = match get_rate {
        0 => &[0, 1, 1],
        1 => &[2, 1, 1],
        _ => &[18, 1, 1],
    };
    let op_dist = WeightedIndex::new(op_weights).unwrap();

    let output_name = &m
        .value_of("output")
        .map_or(ds.to_string() + "_results.csv", |o| o.to_string());
    let output = match OpenOptions::new()
        .read(true)
        .write(true)
        .append(true)
        .open(output_name)
    {
        Ok(f) => csv::Writer::from_writer(f),
        Err(_) => {
            let f = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .open(output_name)
                .unwrap();
            let mut output = csv::Writer::from_writer(f);
            // NOTE: `write_record` on `bench`
            output
                .write_record(&[
                    // "timestamp",
                    "ds",
                    "mm",
                    "threads",
                    "sampling_period",
                    "non_coop",
                    "get_rate",
                    "ops_per_cs",
                    "throughput",
                    "peak_mem",
                    "avg_mem",
                ])
                .unwrap();
            output.flush().unwrap();
            output
        }
    };
    let config = Config {
        ds,
        mm,
        threads,

        aux_thread: if sampling || non_coop > 0 { 1 } else { 0 },
        aux_thread_period: Duration::from_millis(1),
        non_coop,
        non_coop_period: if non_coop == 1 {
            Duration::from_millis(10)
        } else {
            // No repin if -nn or none
            Duration::from_secs(interval)
        },
        sampling,
        sampling_period: Duration::from_millis(sampling_period),

        get_rate,
        op_dist,
        key_dist,
        prefill,
        interval,
        duration,
        ops_per_cs,

        epoch_mib: jemalloc_ctl::epoch::mib().unwrap(),
        allocated_mib: jemalloc_ctl::stats::allocated::mib().unwrap(),
    };
    (config, output)
}

fn bench<N: Unsigned>(config: &Config, output: &mut Writer<File>) {
    println!("{}: {}, {} threads", config.ds, config.mm, config.threads);
    let (ops_per_sec, peak_mem, avg_mem) = match config.mm {
        MM::NR => match config.ds {
            DS::List => bench_nr::<ebr::List<String, String>>(config, PrefillStrategy::Decreasing),
            DS::HashMap => {
                bench_nr::<ebr::HashMap<String, String>>(config, PrefillStrategy::Decreasing)
            }
            DS::NMTree => {
                bench_nr::<ebr::NMTreeMap<String, String>>(config, PrefillStrategy::Random)
            }
            DS::BonsaiTree => {
                bench_nr::<ebr::BonsaiTreeMap<String, String>>(config, PrefillStrategy::Random)
            }
        },
        MM::EBR => match config.ds {
            DS::List => {
                bench_ebr::<ebr::List<String, String>, N>(config, PrefillStrategy::Decreasing)
            }
            DS::HashMap => {
                bench_ebr::<ebr::HashMap<String, String>, N>(config, PrefillStrategy::Decreasing)
            }
            DS::NMTree => {
                bench_ebr::<ebr::NMTreeMap<String, String>, N>(config, PrefillStrategy::Random)
            }
            DS::BonsaiTree => {
                bench_ebr::<ebr::BonsaiTreeMap<String, String>, N>(config, PrefillStrategy::Random)
            }
        },
        MM::PEBR => match config.ds {
            DS::List => {
                bench_pebr::<pebr::List<String, String>, N>(config, PrefillStrategy::Decreasing)
            }
            DS::HashMap => {
                bench_pebr::<pebr::HashMap<String, String>, N>(config, PrefillStrategy::Decreasing)
            }
            DS::NMTree => {
                bench_pebr::<pebr::NMTreeMap<String, String>, N>(config, PrefillStrategy::Random)
            }
            DS::BonsaiTree => bench_pebr::<pebr::BonsaiTreeMap<String, String>, N>(
                config,
                PrefillStrategy::Random,
            ),
        },
    };
    output
        .write_record(&[
            // chrono::Local::now().to_rfc3339(),
            config.ds.to_string(),
            config.mm.to_string(),
            config.threads.to_string(),
            config.sampling_period.as_millis().to_string(),
            config.non_coop.to_string(),
            config.get_rate.to_string(),
            config.ops_per_cs.to_string(),
            ops_per_sec.to_string(),
            peak_mem.to_string(),
            avg_mem.to_string(),
        ])
        .unwrap();
    output.flush().unwrap();
    println!(
        "ops/s: {}, peak mem: {}, avg_mem: {}",
        ops_per_sec, peak_mem, avg_mem
    );
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PrefillStrategy {
    Random,
    Decreasing,
}

impl PrefillStrategy {
    fn prefill_ebr<M: ebr::ConcurrentMap<String, String> + Send + Sync>(
        self,
        config: &Config,
        map: &M,
    ) {
        let guard = unsafe { crossbeam_ebr::unprotected() };
        let mut rng = rand::thread_rng();
        match self {
            PrefillStrategy::Random => {
                for _ in 0..config.prefill {
                    let key = config.key_dist.sample(&mut rng).to_string();
                    let value = key.clone();
                    map.insert(key, value, guard);
                }
            }
            PrefillStrategy::Decreasing => {
                let mut keys = Vec::with_capacity(config.prefill);
                for _ in 0..config.prefill {
                    keys.push(config.key_dist.sample(&mut rng));
                }
                keys.sort_by(|a, b| b.cmp(a));
                for k in keys.drain(..) {
                    let key = k.to_string();
                    let value = key.clone();
                    map.insert(key, value, guard);
                }
            }
        }
        println!("prefilled");
    }

    fn prefill_pebr<M: pebr::ConcurrentMap<String, String> + Send + Sync>(
        self,
        config: &Config,
        map: &M,
    ) {
        let guard = unsafe { crossbeam_pebr::unprotected() };
        let mut handle = M::handle(guard);
        let mut rng = rand::thread_rng();
        match self {
            PrefillStrategy::Random => {
                for _ in 0..config.prefill {
                    let key = config.key_dist.sample(&mut rng).to_string();
                    let value = key.clone();
                    map.insert(&mut handle, key, value, guard);
                }
            }
            PrefillStrategy::Decreasing => {
                let mut keys = Vec::with_capacity(config.prefill);
                for _ in 0..config.prefill {
                    keys.push(config.key_dist.sample(&mut rng));
                }
                keys.sort_by(|a, b| b.cmp(a));
                for k in keys.drain(..) {
                    let key = k.to_string();
                    let value = key.clone();
                    map.insert(&mut handle, key, value, guard);
                }
            }
        }
        println!("prefilled");
    }
}

// TODO: too much duplication
fn bench_nr<M: ebr::ConcurrentMap<String, String> + Send + Sync>(
    config: &Config,
    strategy: PrefillStrategy,
) -> (u64, usize, usize) {
    let map = &M::new();
    strategy.prefill_ebr(config, map);

    let barrier = &Arc::new(Barrier::new(config.threads + config.aux_thread));
    let (ops_sender, ops_receiver) = mpsc::channel();
    let (mem_sender, mem_receiver) = mpsc::channel();

    scope(|s| {
        for _ in 0..config.aux_thread {
            let mem_sender = mem_sender.clone();
            s.spawn(move |_| {
                assert!(config.sampling);
                let mut samples = 0usize;
                let mut acc = 0usize;
                let mut peak = 0usize;
                barrier.clone().wait();

                let start = Instant::now();
                let mut next_sampling = start + config.sampling_period;
                while start.elapsed() < config.duration {
                    let now = Instant::now();
                    if now > next_sampling {
                        config.epoch_mib.advance().unwrap();
                        let allocated = config.allocated_mib.read().unwrap();
                        samples += 1;
                        acc += allocated;
                        peak = max(peak, allocated);
                        next_sampling = now + config.sampling_period;
                    }
                    std::thread::sleep(config.aux_thread_period);
                }
                mem_sender.send((peak, acc / samples)).unwrap();
            });
        }

        for _ in 0..config.threads {
            let ops_sender = ops_sender.clone();
            s.spawn(move |_| {
                let mut rng = rand::thread_rng();
                let c = barrier.clone();

                let mut ops: u64 = 0;

                c.wait();
                let start = Instant::now();

                while start.elapsed() < config.duration {
                    let key = config.key_dist.sample(&mut rng).to_string();
                    match Op::OPS[config.op_dist.sample(&mut rng)] {
                        Op::Get => {
                            map.get(&key, unsafe { crossbeam_ebr::unprotected() });
                        }
                        Op::Insert => {
                            let value = key.clone();
                            map.insert(key, value, unsafe { crossbeam_ebr::unprotected() });
                        }
                        Op::Remove => {
                            map.remove(&key, unsafe { crossbeam_ebr::unprotected() });
                        }
                    }
                    ops += 1;
                }

                ops_sender.send(ops).unwrap();
            });
        }
    })
    .unwrap();

    let mut ops = 0;
    for _ in 0..config.threads {
        let local_ops = ops_receiver.recv().unwrap();
        ops += local_ops;
    }
    let ops_per_sec = ops / config.interval;
    let (peak_mem, avg_mem) = mem_receiver.recv().unwrap();
    (ops_per_sec, peak_mem, avg_mem)
}

fn bench_ebr<M: ebr::ConcurrentMap<String, String> + Send + Sync, N: Unsigned>(
    config: &Config,
    strategy: PrefillStrategy,
) -> (u64, usize, usize) {
    let map = &M::new();
    strategy.prefill_ebr(config, map);

    let collector = &crossbeam_ebr::Collector::new();

    let barrier = &Arc::new(Barrier::new(config.threads + config.aux_thread));
    let (ops_sender, ops_receiver) = mpsc::channel();
    let (mem_sender, mem_receiver) = mpsc::channel();

    scope(|s| {
        // sampling & interference thread
        if config.aux_thread > 0 {
            let mem_sender = mem_sender.clone();
            s.spawn(move |_| {
                let mut samples = 0usize;
                let mut acc = 0usize;
                let mut peak = 0usize;
                let handle = collector.register();
                barrier.clone().wait();

                let start = Instant::now();
                // Immediately drop if no non-coop else keep it and repin periodically.
                let mut guard = ManuallyDrop::new(handle.pin());
                if config.non_coop == 0 {
                    unsafe { ManuallyDrop::drop(&mut guard) };
                }
                let mut next_sampling = start + config.sampling_period;
                let mut next_repin = start + config.non_coop_period;
                while start.elapsed() < config.duration {
                    let now = Instant::now();
                    if now > next_sampling {
                        config.epoch_mib.advance().unwrap();
                        let allocated = config.allocated_mib.read().unwrap();
                        samples += 1;
                        acc += allocated;
                        peak = max(peak, allocated);
                        next_sampling = now + config.sampling_period;
                    }
                    if now > next_repin {
                        (*guard).repin();
                        next_repin = now + config.non_coop_period;
                    }
                    std::thread::sleep(config.aux_thread_period);
                }

                if config.non_coop > 0 {
                    unsafe { ManuallyDrop::drop(&mut guard) };
                }

                if config.sampling {
                    mem_sender.send((peak, acc / samples)).unwrap();
                } else {
                    mem_sender.send((0, 0)).unwrap();
                }
            });
        } else {
            mem_sender.send((0, 0)).unwrap();
        }

        for _ in 0..config.threads {
            let ops_sender = ops_sender.clone();
            s.spawn(move |_| {
                let mut rng = rand::thread_rng();
                let handle = collector.register();
                let c = barrier.clone();

                let mut ops: u64 = 0;

                c.wait();
                let start = Instant::now();

                let mut guard = handle.pin();
                while start.elapsed() < config.duration {
                    let key = config.key_dist.sample(&mut rng).to_string();
                    match Op::OPS[config.op_dist.sample(&mut rng)] {
                        Op::Get => {
                            map.get(&key, &guard);
                        }
                        Op::Insert => {
                            let value = key.clone();
                            map.insert(key, value, &guard);
                        }
                        Op::Remove => {
                            map.remove(&key, &guard);
                        }
                    }
                    ops += 1;
                    if ops % N::to_u64() == 0 {
                        drop(guard);
                        guard = handle.pin();
                    }
                }

                ops_sender.send(ops).unwrap();
            });
        }
    })
    .unwrap();

    let mut ops = 0;
    for _ in 0..config.threads {
        let local_ops = ops_receiver.recv().unwrap();
        ops += local_ops;
    }
    let ops_per_sec = ops / config.interval;
    let (peak_mem, avg_mem) = mem_receiver.recv().unwrap();
    (ops_per_sec, peak_mem, avg_mem)
}

fn bench_pebr<M: pebr::ConcurrentMap<String, String> + Send + Sync, N: Unsigned>(
    config: &Config,
    strategy: PrefillStrategy,
) -> (u64, usize, usize) {
    let map = &M::new();
    strategy.prefill_pebr(config, map);

    let collector = &crossbeam_pebr::Collector::new();

    let barrier = &Arc::new(Barrier::new(config.threads + config.aux_thread));
    let (ops_sender, ops_receiver) = mpsc::channel();
    let (mem_sender, mem_receiver) = mpsc::channel();

    scope(|s| {
        // sampling & interference thread
        if config.aux_thread > 0 {
            let mem_sender = mem_sender.clone();
            s.spawn(move |_| {
                let mut samples = 0usize;
                let mut acc = 0usize;
                let mut peak = 0usize;
                let handle = collector.register();
                barrier.clone().wait();

                let start = Instant::now();
                // Immediately drop if no non-coop else keep it and repin periodically.
                let mut guard = ManuallyDrop::new(handle.pin());
                if config.non_coop == 0 {
                    unsafe { ManuallyDrop::drop(&mut guard) };
                }
                let mut next_sampling = start + config.sampling_period;
                let mut next_repin = start + config.non_coop_period;
                while start.elapsed() < config.duration {
                    let now = Instant::now();
                    if now > next_sampling {
                        config.epoch_mib.advance().unwrap();
                        let allocated = config.allocated_mib.read().unwrap();
                        samples += 1;
                        acc += allocated;
                        peak = max(peak, allocated);
                        next_sampling = now + config.sampling_period;
                    }
                    if now > next_repin {
                        (*guard).repin();
                        next_repin = now + config.non_coop_period;
                    }
                    std::thread::sleep(config.aux_thread_period);
                }

                if config.non_coop > 0 {
                    unsafe { ManuallyDrop::drop(&mut guard) };
                }

                if config.sampling {
                    mem_sender.send((peak, acc / samples)).unwrap();
                } else {
                    mem_sender.send((0, 0)).unwrap();
                }
            });
        } else {
            mem_sender.send((0, 0)).unwrap();
        }

        for _ in 0..config.threads {
            let ops_sender = ops_sender.clone();
            s.spawn(move |_| {
                let mut rng = rand::thread_rng();
                let handle = collector.register();
                let mut map_handle = M::handle(&handle.pin());
                let c = barrier.clone();

                let mut ops: u64 = 0;

                c.wait();
                let start = Instant::now();

                // TODO: repin freq opt?
                let mut guard = handle.pin();
                while start.elapsed() < config.duration {
                    let key = config.key_dist.sample(&mut rng).to_string();
                    match Op::OPS[config.op_dist.sample(&mut rng)] {
                        Op::Get => {
                            map.get(&mut map_handle, &key, &mut guard);
                        }
                        Op::Insert => {
                            let value = key.clone();
                            map.insert(&mut map_handle, key, value, &mut guard);
                        }
                        Op::Remove => {
                            map.remove(&mut map_handle, &key, &mut guard);
                        }
                    }
                    ops += 1;
                    if ops % N::to_u64() == 0 {
                        M::clear(&mut map_handle);
                        guard.repin();
                    }
                }

                ops_sender.send(ops).unwrap();
            });
        }
    })
    .unwrap();

    let mut ops = 0;
    for _ in 0..config.threads {
        let local_ops = ops_receiver.recv().unwrap();
        ops += local_ops;
    }
    let ops_per_sec = ops / config.interval;
    let (peak_mem, avg_mem) = mem_receiver.recv().unwrap();
    (ops_per_sec, peak_mem, avg_mem)
}
