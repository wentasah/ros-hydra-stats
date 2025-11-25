use clap::Parser;

#[derive(Parser)]
#[command(version, about, long_about = None)]
pub struct Cli {
    /// Evaluation ID to analyze
    pub(crate) eval_id: usize,

    /// Print statistics about evaluation failures
    #[arg(long)]
    pub(crate) eval_failures: bool,

    /// List successfully built derivations
    #[arg(long)]
    pub(crate) list_successful: bool,
}
