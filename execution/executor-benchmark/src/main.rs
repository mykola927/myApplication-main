// Copyright (c) Aptos
// SPDX-License-Identifier: Apache-2.0

use aptos_config::config::StoragePrunerConfig;
use aptos_secure_push_metrics::MetricsPusher;
use aptos_vm::AptosVM;
use std::path::PathBuf;
use structopt::StructOpt;

#[global_allocator]
static ALLOC: jemallocator::Jemalloc = jemallocator::Jemalloc;

#[derive(Debug, StructOpt)]
struct Opt {
    #[structopt(long, default_value = "500")]
    block_size: usize,

    #[structopt(long)]
    concurrency_level: Option<usize>,

    #[structopt(subcommand)]
    cmd: Command,

    #[structopt(
        long,
        about = "Verify sequence number of all the accounts after execution finishes"
    )]
    verify_sequence_numbers: bool,
}

impl Opt {
    fn concurrency_level(&self) -> usize {
        match self.concurrency_level {
            None => {
                let level = num_cpus::get();
                println!(
                    "\nVM concurrency level defaults to num of cpus: {}\n",
                    level
                );
                level
            }
            Some(level) => level,
        }
    }
}

#[derive(Debug, StructOpt)]
enum Command {
    CreateDb {
        #[structopt(long, parse(from_os_str))]
        data_dir: PathBuf,

        #[structopt(long, default_value = "1000000")]
        num_accounts: usize,

        #[structopt(long, default_value = "1000000")]
        init_account_balance: u64,

        #[structopt(long)]
        state_store_prune_window: Option<u64>,

        #[structopt(long)]
        default_store_prune_window: Option<u64>,
    },
    RunExecutor {
        #[structopt(
            long,
            default_value = "1000",
            about = "number of transfer blocks to run"
        )]
        blocks: usize,

        #[structopt(long, parse(from_os_str))]
        data_dir: PathBuf,

        #[structopt(long, parse(from_os_str))]
        checkpoint_dir: PathBuf,
    },
}

fn main() {
    let _mp = MetricsPusher::start();
    let opt = Opt::from_args();

    rayon::ThreadPoolBuilder::new()
        .thread_name(|index| format!("rayon-global-{}", index))
        .build_global()
        .expect("Failed to build rayon global thread pool.");
    AptosVM::set_concurrency_level_once(opt.concurrency_level());

    match opt.cmd {
        Command::CreateDb {
            data_dir,
            num_accounts,
            init_account_balance,
            state_store_prune_window,
            default_store_prune_window,
        } => {
            executor_benchmark::db_generator::run(
                num_accounts,
                init_account_balance,
                opt.block_size,
                data_dir,
                StoragePrunerConfig::new(
                    Some(state_store_prune_window.unwrap_or(1_000_000)),
                    Some(default_store_prune_window.unwrap_or(10_000_000)),
                    10_000,
                ),
                opt.verify_sequence_numbers,
            );
        }
        Command::RunExecutor {
            blocks,
            data_dir,
            checkpoint_dir,
        } => {
            aptos_logger::Logger::new().init();
            executor_benchmark::run_benchmark(
                opt.block_size,
                blocks,
                data_dir,
                checkpoint_dir,
                opt.verify_sequence_numbers,
            );
        }
    }
}
