use anyhow::{bail, Context};
use futures::join;
use futures::stream::{self, StreamExt};
use indicatif::{MultiProgress, ProgressBar, ProgressIterator, ProgressStyle};
use regex::Regex;
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
        loop {
            let mut was_cached = false;
            let json_str = if let Ok(cached) = fs::read_to_string(&cache_path).await {
                was_cached = true;
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
                fs::write(&cache_path, &data).await?;
                data
            };
            let json: JsonValue = serde_json::from_str(&json_str)?;
            if path.starts_with("build/") && was_cached && json["finished"].as_i64().unwrap() == 0 {
                // Retry cached unfinished builds once
                fs::remove_file(&cache_path).await?;
                continue;
            }
            return Ok(json);
        }
    }
}

async fn nix_eval_jobs(
    tarball: &str,
    system: &str,
    cross_system: Option<&str>,
    pb: &ProgressBar,
) -> anyhow::Result<Vec<JsonValue>> {
    let cache_path = format!(
        "{CACHE_DIR}/{}_{system}_{cross_system:?}.jsonl",
        tarball.replace("/", "_"),
    );
    let jobs = if let Ok(cached) = fs::read_to_string(&cache_path).await {
        cached
            .lines()
            .into_iter()
            .map(|line| serde_json::from_str(line))
            .collect::<Result<Vec<_>, _>>()?
    } else {
        pb.set_message("Starting...");
        let mut nix_eval_jobs = Command::new("nix-eval-jobs");
        nix_eval_jobs
            .args([
                "--show-input-drvs",
                "--expr",
                format!("(import (fetchTarball \"{tarball}\") {{ system = \"{system}\";{} }}).rosPackages", cross_system
                        .map(|s| format!(" crossSystem = {s};")).unwrap_or("".to_string())).as_str(),
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

fn print_eval_failure_summary(jobs: &Vec<JsonValue>) {
    let re_missing = Regex::new(r"error: attribute '[^']*' missing").unwrap();
    let re_broken = Regex::new(
        r"error: Package ‘[^’]*’( in /nix/store[^ ]*) is marked as (broken|insecure), refusing to evaluate",
    )
    .unwrap();
    let re_unfree = Regex::new(
        r"error: Package ‘[^’]*’( in /nix/store[^ ]*) has an unfree license \(‘[^’]*’\), refusing to evaluate",
    )
    .unwrap();
    let re_missing_arg =
        Regex::new(r#"callPackageWith: Function called without required argument "[^"]*""#)
            .unwrap();
    let mut eval_failure_reasons: HashMap<String, Vec<&str>> = HashMap::new();
    for job in jobs {
        if let Some(JsonValue::String(error)) = job.get("error") {
            let attr = job["attr"].as_str().unwrap();
            print!("{attr}: ");
            let mut cnt = 0;
            let mut record_reason = |reason: &str| {
                cnt += 1;
                eval_failure_reasons
                    .entry(reason.to_string())
                    .or_insert(Vec::new())
                    .push(attr);
            };
            for m in re_missing.find_iter(error) {
                record_reason(m.as_str());
            }
            for cap in re_broken.captures_iter(error) {
                record_reason(&cap[0].replace(&cap[1], ""));
            }
            for cap in re_unfree.captures_iter(error) {
                record_reason(&cap[0].replace(&cap[1], ""));
            }
            for m in re_missing_arg.find_iter(error) {
                record_reason(m.as_str());
            }
            if cnt == 0 {
                println!("{}", indent::indent_all_by(4, error));
            }
        }
    }
    for (reason, attrs) in &eval_failure_reasons {
        println!("{reason}: {}", attrs.len());
        for attr in attrs {
            println!("    {attr}");
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = std::env::args();
    let args = args;
    let mut args = args;
    let prog = args.next().context("Prog name")?;
    let eval_id = args
        .next()
        .context(format!("Usage: {prog} <eval_id>"))?
        .parse::<usize>()
        .context("Invalid eval id")?;

    let hydra = Arc::new(Hydra::new());

    eprintln!("Fetching hydra evaluation {eval_id}...");
    let eval = hydra.get(format!("eval/{eval_id}").as_str()).await?;

    let mp = MultiProgress::new();

    let builds = eval["builds"].as_array().expect("builds is not an array");
    let hydra_builds_future = stream::iter(
        builds
            .into_iter()
            .map(|build| {
                let build_id = build.as_u64().expect("build_id not u64");
                let hydra = hydra.clone();
                async move { hydra.get(format!("build/{build_id}").as_str()).await }
            })
            .progress_with(
                mp.add(
                    ProgressBar::new(builds.len() as u64)
                        .with_style(ProgressStyle::with_template(
                            "{prefix} {wide_bar} {pos}/{len}",
                        )?)
                        .with_prefix("Downloading builds:"),
                ),
            ),
    )
    .buffer_unordered(10)
    .collect::<Vec<_>>();

    let url = eval["jobsetevalinputs"]["nix-ros-overlay"]["uri"]
        .as_str()
        .expect("No nix-ros-overlay.uri in eval");
    let rev = eval["jobsetevalinputs"]["nix-ros-overlay"]["revision"]
        .as_str()
        .expect("No nix-ros-overlay.revision in eval");
    let tarball = format!("{url}/archive/{rev}.tar.gz");
    let system = eval["jobsetevalinputs"]["system"]["value"]
        .as_str()
        .unwrap();
    let cross_system = eval["jobsetevalinputs"]["crossSystem"]["value"].as_str();

    eprintln!("Tarball for evaluation: {tarball}");
    let pb = mp.add(
        ProgressBar::new_spinner()
            .with_style(
                ProgressStyle::with_template("{prefix} {spinner} {msg}")?
                    .tick_chars("🕛🕐🕑🕒🕓🕔🕕🕖🕗🕘🕙🕚"),
            )
            .with_prefix("Evaluating:"),
    );

    let (hydra_builds, jobs) = join!(
        hydra_builds_future,
        nix_eval_jobs(&tarball, system, cross_system, &pb)
    );
    mp.clear()?;

    let hydra_builds = hydra_builds
        .into_iter()
        .collect::<anyhow::Result<Vec<_>>>()?;
    let jobs = jobs?;

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

    print_eval_failure_summary(&jobs);
    Ok(())
}
