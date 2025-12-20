use anyhow::bail;
use clap::Parser;
use futures::future::join_all;
use futures::join;
use futures::stream::{self, StreamExt};
use indicatif::{MultiProgress, ProgressBar, ProgressIterator, ProgressStyle};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;
use std::{path::Path, process::Stdio, sync::Arc};
use strum::{EnumIter, EnumProperty, IntoEnumIterator};
use tokio::fs;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;

mod cli;

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
                let url = format!("{HYDRA_URL}/{path}");
                let response = self
                    .client
                    .get(&url)
                    .header("Accept", "application/json")
                    .send()
                    .await?;
                if !response.status().is_success() {
                    bail!("Failure getting {url}: {}", response.status());
                }
                let data = response.text().await?;
                fs::create_dir_all(Path::new(&cache_path).parent().unwrap()).await?;
                fs::write(&cache_path, &data).await?;
                data
            };
            let json: JsonValue = serde_json::from_str(&json_str)?;
            if path.starts_with("build/")
                && was_cached
                && json["finished"].as_i64().unwrap_or(0) == 0
            {
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
            .map(serde_json::from_str)
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

struct EvalErrorAnalyzer {
    missing: Regex,
    broken: Regex,
    unfree: Regex,
    missing_arg: Regex,
}

impl EvalErrorAnalyzer {
    pub fn new() -> Self {
        Self {
            missing: Regex::new(r"error: attribute '[^']*' missing").unwrap(),
            broken : Regex::new(r"error: Package ‘[^’]*’( in /nix/store[^ ]*) is marked as (broken|insecure), refusing to evaluate").unwrap(),
            unfree : Regex::new(r"error: Package ‘[^’]*’( in /nix/store[^ ]*) has an unfree license \(‘[^’]*’\), refusing to evaluate").unwrap(),
            missing_arg: Regex::new(r#"callPackageWith: Function called without required argument "[^"]*""#).unwrap(),
        }
    }
}

fn print_eval_failure_summary(jobs: &Vec<JsonValue>) {
    let re: LazyLock<EvalErrorAnalyzer> = LazyLock::new(EvalErrorAnalyzer::new);
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
                    .or_default()
                    .push(attr);
            };
            for m in re.missing.find_iter(error) {
                record_reason(m.as_str());
            }
            for cap in re.broken.captures_iter(error) {
                record_reason(&cap[0].replace(&cap[1], ""));
            }
            for cap in re.unfree.captures_iter(error) {
                record_reason(&cap[0].replace(&cap[1], ""));
            }
            for m in re.missing_arg.find_iter(error) {
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

struct HydraEval {
    hydra_builds: Vec<JsonValue>,
    eval_jobs: Vec<JsonValue>,
}

#[derive(Deserialize, Copy, Clone)]
struct HydraBuild {
    finished: i8,
    buildstatus: i8,
    id: u64,
}

impl HydraBuild {
    fn success(&self) -> bool {
        self.finished == 1 && self.buildstatus == 0
    }
    fn url(&self) -> String {
        format!("https://hydra.iid.ciirc.cvut.cz/build/{}", self.id)
    }
}

#[derive(
    strum_macros::Display, strum_macros::EnumProperty, Eq, Hash, PartialEq, EnumIter, Clone, Copy,
)]
enum CiChange {
    #[strum(to_string = "Removed")]
    Removed,
    #[strum(to_string = "Introduced eval errors")]
    NewEvalError,
    #[strum(to_string = "Fixed eval errors but build fails")]
    FixedEvalErrorBuildFails,
    #[strum(to_string = "Fixed eval errors")]
    FexedEvalError,
    #[strum(to_string = "Still present eval errors", props(list_attrs = false))]
    EvalErrrorNoChange,
    #[strum(to_string = "Introduced build failures")]
    NewBuildFailure,
    #[strum(to_string = "Fixed build failures")]
    FixedBuildFailure,
    #[strum(to_string = "Still failing builds", props(list_attrs = false))]
    BuildFailureNoChange,
    #[strum(to_string = "Still succeeding builds", props(list_attrs = false))]
    BuildSuccessNoChange,
    #[strum(to_string = "Kept unbuilt attributes", props(list_attrs = false))]
    UnbuiltNoChange,
    #[strum(to_string = "Turns unbuilt attributes into eval error")]
    UnbuiltToEvalError,
    #[strum(to_string = "Starts building previously unbuilt attributes")]
    UnbuiltToBuild,
    #[strum(to_string = "Introduced unbuilt attributes")]
    NewUnbuiltAttr,
}

#[derive(Copy, Clone)]
enum HydraAttrStatus<'a> {
    EvalError(&'a str),
    Build(HydraBuild),
    Unbuilt,
}

struct AttrInfo<'a> {
    attr: String,
    status: Option<HydraAttrStatus<'a>>,
}

impl<'a> HydraAttrStatus<'a> {
    fn compare(&self, other: &HydraAttrStatus) -> CiChange {
        use CiChange::*;
        use HydraAttrStatus::*;
        match (self, other) {
            (Build(_), EvalError(_)) => NewEvalError,
            (EvalError(_), Build(b)) if !b.success() => FixedEvalErrorBuildFails,
            (EvalError(_), Build(_)) => FexedEvalError,
            (EvalError(_), EvalError(_)) => EvalErrrorNoChange,
            (Build(b1), Build(b2)) if b1.success() && !b2.success() => NewBuildFailure,
            (Build(b1), Build(b2)) if !b1.success() && b2.success() => FixedBuildFailure,
            (Build(b1), Build(b2)) if !b1.success() && !b2.success() => BuildFailureNoChange,
            (Build(_), Build(_)) => BuildSuccessNoChange,
            (Unbuilt, Unbuilt) => UnbuiltNoChange,
            (Unbuilt, EvalError(_)) => UnbuiltToEvalError,
            (Unbuilt, Build(_)) => UnbuiltToBuild,
            (_, Unbuilt) => NewUnbuiltAttr,
        }
    }
}

struct HydraEvalSummary<'a>(HashMap<&'a str, HydraAttrStatus<'a>>);

impl<'a> HydraEvalSummary<'a> {
    fn compare(&self, other: &HydraEvalSummary) {
        let mut summary: HashMap<CiChange, Vec<AttrInfo>> = HashMap::new();
        for (&attr, status) in &self.0 {
            let other_status = other.0.get(attr);
            let change = match (status, other_status) {
                (_, None) => CiChange::Removed,
                (_, Some(other)) => status.compare(other),
            };
            summary.entry(change).or_default().push(AttrInfo {
                attr: attr.to_string(),
                status: other_status.copied(),
            });
        }
        for change in CiChange::iter() {
            summary.entry(change).and_modify(|attrs| {
                attrs.sort_by(|a, b| a.attr.cmp(&b.attr));
                println!("\n{change}: {}", attrs.len());
                if change.get_bool("list_attrs").unwrap_or(true) {
                    for ai in attrs {
                        match ai.status {
                            Some(HydraAttrStatus::Build(b)) => {
                                println!("  - [{}]({})", ai.attr, b.url())
                            }
                            _ => println!("  - {}", ai.attr),
                        }
                    }
                }
            });
        }
    }
}

impl HydraEval {
    pub fn new(hydra_builds: Vec<JsonValue>, eval_jobs: Vec<JsonValue>) -> Self {
        Self {
            hydra_builds,
            eval_jobs,
        }
    }

    fn summary(&'_ self) -> HydraEvalSummary<'_> {
        let builds: HashMap<&str, HydraBuild> = self
            .hydra_builds
            .iter()
            .map(|build| {
                (
                    build["job"].as_str().unwrap(),
                    serde_json::from_value(build.clone()).unwrap(),
                )
            })
            .collect();
        HydraEvalSummary(
            self.eval_jobs
                .iter()
                .map(|job| {
                    let attr = job["attr"].as_str().unwrap();
                    job["error"]
                        .as_str()
                        .map(|err| (attr, HydraAttrStatus::EvalError(err)))
                        .or_else(|| {
                            builds
                                .get(format!("rosPackages.{attr}").as_str())
                                .map(|build| (attr, HydraAttrStatus::Build(build.clone())))
                        })
                        .unwrap_or((attr, HydraAttrStatus::Unbuilt))
                })
                .collect(),
        )
    }

    fn compare(&self, other: &Self) {
        self.summary().compare(&other.summary())
    }
}

async fn fetch_hydra_eval(
    hydra: Arc<Hydra>,
    eval_id: usize,
    mp: &MultiProgress,
) -> anyhow::Result<HydraEval> {
    mp.println(format!("Fetching hydra evaluation {eval_id}..."))?;
    let eval = hydra.get(format!("eval/{eval_id}").as_str()).await?;

    let builds = eval["builds"].as_array().expect("builds is not an array");
    let hydra_builds_future = stream::iter(
        builds
            .iter()
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
                        .with_prefix(format!("Downloading builds of eval {eval_id}:")),
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

    mp.println(format!("Tarball for evaluation: {tarball}"))?;
    let pb = mp.add(
        ProgressBar::new_spinner()
            .with_style(
                ProgressStyle::with_template("{prefix} {spinner} {msg}")?
                    .tick_chars("🕛🕐🕑🕒🕓🕔🕕🕖🕗🕘🕙🕚"),
            )
            .with_prefix(format!("Evaluating {eval_id}:")),
    );

    let (hydra_builds, jobs) = join!(
        hydra_builds_future,
        nix_eval_jobs(&tarball, system, cross_system, &pb)
    );
    mp.remove(&pb);

    let hydra_builds = hydra_builds
        .into_iter()
        .collect::<anyhow::Result<Vec<_>>>()?;

    Ok(HydraEval::new(hydra_builds, jobs?))
}

fn process_and_print_eval_stats(hydra_eval: HydraEval, cli: cli::EvalArgs) -> anyhow::Result<()> {
    let mut job_deps = HashMap::<&str, Vec<&str>>::new();
    for job in &hydra_eval.eval_jobs {
        if job.get("inputDrvs").is_none() {
            continue; // eval error
        }
        for input_drv in job["inputDrvs"].as_object().unwrap().keys() {
            job_deps
                .entry(input_drv.as_str())
                .or_default()
                .push(job["drvPath"].as_str().unwrap());
        }
    }

    let failed_builds = hydra_eval
        .hydra_builds
        .iter()
        .filter(|build| {
            build["buildstatus"].as_i64().unwrap_or(
                0, /* queued builds (value null) are not considered failed */
            ) != 0
        })
        .collect::<Vec<_>>();

    if cli.list_successful {
        hydra_eval
            .hydra_builds
            .iter()
            .filter(|build| build["buildstatus"].as_i64().is_some_and(|b| b == 0))
            //.for_each(|build| println!("{}", build["drvpath"].as_str().unwrap()));
            .for_each(|build| println!("{}", build["job"].as_str().unwrap()));
        return Ok(());
    }

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

    if cli.eval_failures {
        print_eval_failure_summary(&hydra_eval.eval_jobs);
    }
    Ok(())
}

async fn handle_pr(hydra: Arc<Hydra>, pr_num: usize, mp: &MultiProgress) -> anyhow::Result<()> {
    let jobsets = join_all(vec![
        hydra.get("jobset/nix-ros-experiments/wentasah-rosdistro-sync/evals"),
        hydra.get("jobset/nix-ros-experiments/wentasah-test/evals"),
        hydra.get("jobset/nix-ros-experiments/lopsided98-develop/evals"),
        // TODO: Reread without cache if eval is not found below
    ])
    .await
    .into_iter()
    .collect::<Result<Vec<_>, _>>()?;

    let gh = Command::new("gh")
        .arg("api")
        .arg(format!("repos/lopsided98/nix-ros-overlay/pulls/{pr_num}"))
        .output()
        .await?;
    let pr: JsonValue = serde_json::from_str(str::from_utf8(&gh.stdout)?)?;
    // println!("{}", &pr);
    let base_sha = pr["base"]["sha"].as_str().unwrap();
    let head_sha = pr["head"]["sha"].as_str().unwrap();

    let find_eval_id_of_commit = |sha: &str| {
        jobsets.iter().find_map(|evals| {
            evals["evals"].as_array().unwrap().iter().find_map(|eval| {
                (eval // wrap line
                 ["jobsetevalinputs"].as_object().unwrap() // wrap line
                 ["nix-ros-overlay"].as_object().unwrap() // wrap line
                 ["revision"].as_str().unwrap() // wrap line
                 == sha)
                    .then_some(eval["id"].as_u64().unwrap())
            })
        })
    };
    let base_eval = find_eval_id_of_commit(base_sha);
    let head_eval = find_eval_id_of_commit(head_sha);

    dbg!(base_eval);
    dbg!(head_eval);

    let evals = join_all(vec![
        fetch_hydra_eval(hydra.clone(), base_eval.unwrap() as usize, mp),
        fetch_hydra_eval(hydra.clone(), head_eval.unwrap() as usize, mp),
    ])
    .await
    .into_iter()
    .collect::<Result<Vec<_>, _>>()?;
    evals[0].compare(&evals[1]);
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = cli::Cli::parse();

    let hydra = Arc::new(Hydra::new());

    let mp = MultiProgress::new();
    match cli.command {
        cli::Commands::Eval(args) => {
            let hydra_eval = fetch_hydra_eval(hydra.clone(), args.eval_id, &mp).await?;
            mp.println("Calculating reverse dependencies...")?;
            process_and_print_eval_stats(hydra_eval, args)?;
        }
        cli::Commands::PR { pr } => {
            handle_pr(hydra.clone(), pr, &mp).await?;
        }
    };
    mp.clear()?;

    Ok(())
}
