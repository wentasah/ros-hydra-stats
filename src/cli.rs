use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(version, about, long_about = None)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Print hydra eval stats
    Eval(EvalArgs),
    /// Produce comparison of PR source and target
    PR { pr: usize },
    /// Produce comparison of two evaluations
    CompareEvals {
        /// Old eval id
        old: u64,
        /// New eval id
        new: u64,
    },
    /// Produce latest evaluations of nix-ros-exoeriments jobsets
    CompareJobsets {
        /// Old jobset (e.g. lopsided98-master)
        old: String,
        /// New jobset (e.g. lopsided98-develop)
        new: String,
        /// Whether to use cached evaluation data (if available) instead of fetching from hydra
        #[arg(long, short = 'c', default_value_t = false)]
        use_cache: bool,
    },
}

#[derive(Args)]
pub struct EvalArgs {
    /// Evaluation ID to analyze
    pub eval_id: u64,

    /// Print statistics about evaluation failures
    #[arg(long)]
    pub eval_failures: bool,

    /// List successfully built derivations
    #[arg(long)]
    pub list_successful: bool,
}
