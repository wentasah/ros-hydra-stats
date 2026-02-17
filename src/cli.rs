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
    Compare {
        /// Old eval id
        old: u64,
        /// New eval id
        new: u64,
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
