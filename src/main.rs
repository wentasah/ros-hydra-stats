use anyhow::bail;
use futures::stream::{self, StreamExt};
use indicatif::{ProgressBar, ProgressIterator, ProgressStyle};
use serde::Serialize;
use serde_json::Value as JsonValue;
use std::collections::{HashMap, HashSet};
use std::{path::Path, process::Stdio, sync::Arc};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::{fs, process::Command};

static HYDRA_URL: &str = "http://10.35.95.5:3000";
static CACHE_DIR: &str = ".cache";

struct Hydra {
    client: reqwest::Client,
}

impl Hydra {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }

    async fn get(&self, path: &str) -> anyhow::Result<JsonValue> {
        let cache_path = format!("{CACHE_DIR}/{path}");
        let json_str = if let Ok(cached) = fs::read_to_string(&cache_path).await {
            cached
        } else {
            let data = self
                .client
                .get(format!("{HYDRA_URL}/{path}"))
                .header("Accept", "application/json")
                .send()
                .await?
                .text()
                .await?;
            fs::create_dir_all(Path::new(&cache_path).parent().unwrap()).await?;
            fs::write(cache_path, &data).await?;
            data
        };
        Ok(serde_json::from_str(&json_str)?)
    }
}

async fn nix_eval_jobs(tarball: &str) -> anyhow::Result<Vec<JsonValue>> {
    let cache_path = format!("{CACHE_DIR}/{}.jsonl", tarball.replace("/", "_"));
    let jobs = if let Ok(cached) = fs::read_to_string(&cache_path).await {
        cached
            .lines()
            .into_iter()
            .map(|line| serde_json::from_str(line))
            .collect::<Result<Vec<_>, _>>()?
    } else {
        let mut nix_eval_jobs = Command::new("nix-eval-jobs");
        nix_eval_jobs
            .args([
                "--show-input-drvs",
                "--expr",
                format!("(import (fetchTarball \"{tarball}\") {{}}).rosPackages").as_str(),
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = nix_eval_jobs.spawn().expect("failed to spawn command");
        let stdout = child.stdout.take().unwrap();
        let mut job_lines = BufReader::new(stdout).lines();
        // Ensure the child process is spawned in the runtime so it can
        // make progress on its own while we await for any output.
        let child_handle = tokio::spawn(async move {
            child
                .wait()
                .await
                .expect("child process encountered an error")
        });

        let pb = ProgressBar::new_spinner()
            .with_style(ProgressStyle::default_spinner().tick_chars("🕛🕐🕑🕒🕓🕔🕕🕖🕗🕘🕙🕚"));

        let mut jobs = vec![];
        let mut to_cache = String::new();
        while let Some(line) = job_lines.next_line().await? {
            let job = serde_json::from_str::<JsonValue>(&line)?;
            to_cache += &line;
            to_cache += "\n";
            pb.inc(1);
            pb.set_message(job["attr"].as_str().unwrap().to_owned());
            jobs.push(job);
        }
        pb.finish();
        let exit_status = child_handle.await?;
        if !exit_status.success() {
            bail!("{nix_eval_jobs:?} failed with {exit_status}");
        }
        fs::create_dir_all(Path::new(&cache_path).parent().unwrap()).await?;
        fs::write(cache_path, &to_cache).await?;
        jobs
    };
    Ok(jobs)
}

fn dependent_job_counts(build: &JsonValue, jobs: &HashMap<&str, Vec<&str>>) -> Vec<usize> {
    let mut deps = HashSet::new();
    deps.insert(build["drvpath"].as_str().unwrap());
    let mut dep_counts = vec![];
    loop {
        let mut new_deps = HashSet::new();
        for dep in &deps {
            for d in jobs.get(dep).unwrap_or(&vec![]) {
                new_deps.insert(*d);
            }
        }
        deps.extend(new_deps);
        if *dep_counts.last().unwrap_or(&0) == deps.len() {
            // Fixpoint found
            break;
        }
        dep_counts.push(deps.len());
    }
    dep_counts
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let eval_id = 6104;

    let hydra = Arc::new(Hydra::new());

    eprintln!("Fetching hydra evaluation...");
    let eval = hydra.get(format!("eval/{eval_id}").as_str()).await?;

    eprintln!("Fetching builds from the evaluation...");
    let hydra_builds = stream::iter(
        eval["builds"]
            .as_array()
            .expect("builds is not an array")
            .into_iter()
            .map(|build| {
                let build_id = build.as_u64().expect("build_id not u64");
                let hydra = hydra.clone();
                async move { hydra.get(format!("build/{build_id}").as_str()).await }
            })
            .progress(),
    )
    .buffer_unordered(10)
    .collect::<Vec<_>>()
    .await
    .into_iter()
    .collect::<anyhow::Result<Vec<_>>>()?;

    let url = eval["jobsetevalinputs"]["nix-ros-overlay"]["uri"]
        .as_str()
        .expect("No nix-ros-overlay.uri in eval");
    let rev = eval["jobsetevalinputs"]["nix-ros-overlay"]["revision"]
        .as_str()
        .expect("No nix-ros-overlay.revision in eval");
    let tarball = format!("{url}/archive/{rev}.tar.gz");

    eprintln!("Evaluating jobs...");
    let jobs = nix_eval_jobs(&tarball).await?;

    eprintln!("Calculating reverse dependencies...");
    let mut job_deps = HashMap::<&str, Vec<&str>>::new();
    for job in &jobs {
        if job.get("inputDrvs").is_none() {
            continue; // eval error
        }
        for input_drv in job["inputDrvs"].as_object().unwrap().keys() {
            job_deps
                .entry(input_drv.as_str())
                .or_insert(Vec::<&str>::new())
                .push(job["drvPath"].as_str().unwrap());
        }
    }

    let failed_builds = hydra_builds
        .iter()
        .filter(|build| build["buildstatus"].as_i64().unwrap() != 0)
        .collect::<Vec<_>>();

    #[derive(Serialize, Debug)]
    struct Job<'a> {
        job: &'a str,
        direct_deps: usize,
        all_deps: usize,
        build_url: String,
    }

    let mut failed_jobs = Vec::new();
    for b in failed_builds {
        let cnts = dependent_job_counts(b, &job_deps);
        failed_jobs.push(Job {
            job: b["job"].as_str().unwrap(),
            direct_deps: *cnts.first().unwrap_or(&0),
            all_deps: *cnts.last().unwrap_or(&0),
            build_url: format!("{HYDRA_URL}/build/{}", b["id"]),
        });
    }

    failed_jobs.sort_by_key(|job| job.all_deps);

    for fj in &failed_jobs {
        println!("{}", serde_json::to_string(fj)?);
    }

    Ok(())
}
