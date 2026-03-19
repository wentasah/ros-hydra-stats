use clap::CommandFactory;
use clap::ValueEnum;
use clap_complete::{Shell, generate_to};
use std::env;
use std::io::Error;

include!("src/cli.rs");

fn main() -> Result<(), Error> {
    let outdir = match env::var_os("OUT_DIR") {
        None => return Ok(()),
        Some(outdir) => outdir,
    };

    let mut cmd = Cli::command();
    let bin_name = cmd.get_name().to_string();
    for &shell in Shell::value_variants() {
        let path = generate_to(shell, &mut cmd, &bin_name, &outdir)?;
        println!("cargo:warning=completion file is generated: {path:?}");
    }

    Ok(())
}
