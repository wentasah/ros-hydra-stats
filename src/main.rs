// Copyright (C) 2026 Michal Sojka
// SPDX-License-Identifier: AGPL-3.0

use anyhow::bail;
use clap::Parser;
use futures::future::join_all;
use futures::join;
use futures::stream::{self, StreamExt};
use indicatif::{MultiProgress, ProgressBar, ProgressIterator, ProgressStyle};
use itertools::Itertools;
use log::warn;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::cell::OnceCell;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::Duration;
use std::{path::Path, process::Stdio, sync::Arc};
use strum::{EnumIter, EnumProperty, IntoEnumIterator};
use tokio::fs;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::time::sleep;

mod cli;

static HYDRA_URL: &str = "http://10.35.95.5:3000";
static CACHE_DIR: LazyLock<PathBuf> = LazyLock::new(|| {
    dirs::cache_dir()
        .expect("Expecting cache directory")
        .join("ros-hydra-stats")
});

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
        self.get_with_cachectrl(path, true).await
    }

    async fn get_with_cachectrl(&self, path: &str, use_cache: bool) -> anyhow::Result<JsonValue> {
        let cache_path = CACHE_DIR.join(path);
        let mut failure_cnt = 0;
        loop {
            let mut was_cached = false;
            let json_str = if use_cache && let Ok(cached) = fs::read_to_string(&cache_path).await {
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
                    failure_cnt += 1;
                    if failure_cnt < 10 {
                        sleep(Duration::from_millis(300)).await;
                        continue;
                    }
                    bail!("Failure getting {url}: {}", response.status());
                }
                let data = response.text().await?;
                fs::create_dir_all(Path::new(&cache_path).parent().unwrap()).await?;
                fs::write(&cache_path, &data).await?;
                data
            };
            let json: JsonValue = serde_json::from_str(&json_str)?;
            let finished = json["finished"].as_i64().unwrap_or(0) == 1;
            let aborted = json["buildstatus"].as_i64().unwrap_or(3) == 3;
            if path.starts_with("build/") && was_cached && (!finished || aborted) {
                // Retry cached unfinished builds once
                fs::remove_file(&cache_path).await?;
                continue;
            }
            return Ok(json);
        }
    }
}

async fn nix_eval_jobs(
    release_nix_tarball_url: &str,
    tarball_url: &str,
    system: &str,
    distro: Option<&str>,
    cross_system: Option<&str>,
    pb: &ProgressBar,
) -> anyhow::Result<Vec<JsonValue>> {
    let cache_path = CACHE_DIR.join(format!(
        "{}_{}_{system}_{distro:?}_{cross_system:?}.jsonl",
        release_nix_tarball_url.replace("/", "_"),
        tarball_url.replace("/", "_")
    ));
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
                "--force-recurse",
                "--show-input-drvs",
                "--expr",
                format!(
                    r#"
import ''${{fetchTarball "{release_nix_tarball_url}"}}/release.nix'' {{
  nix-ros-overlay = (fetchTarball "{tarball_url}");
  system = "{system}";
  {}
}}"#,
                    cross_system
                        .map(|s| format!(" crossSystem = {s};"))
                        .unwrap_or("".to_string())
                )
                .as_str(),
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

fn dependent_job_counts(drv: &DrvPath, jobs: &JobDeps) -> Vec<usize> {
    let mut deps = HashSet::new();
    deps.insert(drv);
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
    other: Regex,
}

impl EvalErrorAnalyzer {
    pub fn new() -> Self {
        Self {
            missing: Regex::new(r"error: attribute '[^']*' missing").unwrap(),
            broken : Regex::new(r"error: Package ‘[^’]*’( in /nix/store[^ ]*) is marked as (broken|insecure), refusing to evaluate").unwrap(),
            unfree : Regex::new(r"error: Package ‘[^’]*’( in /nix/store[^ ]*) has an unfree license \(‘[^’]*’\), refusing to evaluate").unwrap(),
            missing_arg: Regex::new(r#"callPackageWith: Function called without required argument "[^"]*""#).unwrap(),
            other: Regex::new(r"    (error: .*)").unwrap(),
        }
    }
    pub fn analyze(&self, error: &str) -> Option<String> {
        if let Some(mtch) = self.missing.find(error) {
            return Some(mtch.as_str().to_string());
        }
        if let Some(cap) = self.broken.captures(error) {
            return Some(cap[0].replace(&cap[1], ""));
        }
        if let Some(cap) = self.unfree.captures(error) {
            return Some(cap[0].replace(&cap[1], ""));
        }
        if let Some(mtch) = self.missing_arg.find(error) {
            return Some(mtch.as_str().to_string());
        }
        if let Some(cap) = self.other.captures(error) {
            return Some(cap[1].to_string());
        }
        None
    }
}

static EVAL_ERROR_ANALYZER: LazyLock<EvalErrorAnalyzer> = LazyLock::new(EvalErrorAnalyzer::new);

fn print_eval_failure_summary(jobs: &Vec<JsonValue>) {
    let mut eval_failure_reasons: HashMap<String, Vec<&str>> = HashMap::new();
    for job in jobs {
        if let Some(JsonValue::String(error)) = job.get("error") {
            let attr = job["attr"].as_str().unwrap();
            if let Some(reason) = EVAL_ERROR_ANALYZER.analyze(error) {
                eval_failure_reasons
                    .entry(reason.to_string())
                    .or_default()
                    .push(attr);
            } else {
                println!("{attr}: ");
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
    eval_id: u64,
    distro: Option<String>,
    hydra_builds: Vec<JsonValue>,
    eval_jobs: Vec<JsonValue>,
}

#[derive(Deserialize, Clone)]
struct HydraBuild {
    drvpath: String,
    finished: i8,
    buildstatus: i8,
    id: u64,
}

#[derive(Copy, Clone)]
struct EvalInfo {
    direct_deps: usize,
    all_deps: usize,
}

#[derive(Clone)]
struct BuildInfo {
    eval: EvalInfo,
    hydra: HydraBuild,
}

impl HydraBuild {
    fn success(&self) -> bool {
        self.finished == 1 && self.buildstatus == 0
    }
    fn aborted(&self) -> bool {
        self.finished == 1 && self.buildstatus == 3
    }
    fn url(&self) -> String {
        format!("https://hydra.iid.ciirc.cvut.cz/build/{}", self.id)
    }
}

#[derive(
    strum_macros::Display, strum_macros::EnumProperty, Eq, Hash, PartialEq, EnumIter, Clone, Copy,
)]
enum CiChange {
    #[strum(to_string = "✅ Added successfully")]
    AddedOk,
    #[strum(to_string = "✅ Removed")]
    Removed,
    #[strum(to_string = "✅ Fixed eval errors")]
    FixedEvalError,
    #[strum(to_string = "✅ Fixed build failures")]
    FixedBuildFailure,
    #[strum(to_string = "✅ Still succeeding builds", props(list_attrs = false))]
    BuildSuccessNoChange,
    #[strum(to_string = "✅ Still unbuilt attributes", props(list_attrs = false))]
    UnbuiltNoChange,
    #[strum(to_string = "✅ Starts building previously unbuilt attributes successfully")]
    UnbuiltToBuildOk,
    #[strum(to_string = "⚠️ Added unbuilt attributes")]
    AddedUnbuilt,
    #[strum(
        to_string = "⚠️ Still present eval errors",
        props(list_attrs = false, summary = true)
    )]
    EvalErrrorNoChange,
    #[strum(to_string = "⚠️ Still failing builds", props(list_attrs = false))]
    BuildFailureNoChange,
    #[strum(to_string = "⚠️ Introduced unbuilt attributes")]
    NewUnbuiltAttr,
    #[strum(to_string = "❌ Added with eval errors")]
    AddedEvalError,
    #[strum(to_string = "❌ Added with build failures")]
    AddedBuildFailure,
    #[strum(
        to_string = "❌ Introduced eval errors",
        props(list_attrs = false, summary = true)
    )]
    NewEvalError,
    #[strum(to_string = "❌ Fixed eval errors but build fails")]
    FixedEvalErrorBuildFails,
    #[strum(to_string = "❌ Introduced build failures")]
    NewBuildFailure,
    #[strum(to_string = "❌ Turns unbuilt attributes into eval errors")]
    UnbuiltToEvalError,
    #[strum(to_string = "❌ Starts building previously unbuilt attributes with build failures")]
    UnbuiltToBuildFailure,
}

#[derive(Clone)]
enum HydraAttrStatus<'a> {
    EvalError(&'a str),
    Build(BuildInfo),
    Unbuilt,
}

struct AttrInfo<'a> {
    attr: String,
    status: Option<HydraAttrStatus<'a>>,
}

impl<'a> AttrInfo<'a> {
    fn ros_index_url(&self, eval_distro: Option<&str>) -> Option<String> {
        let (distro, pkg) = match eval_distro {
            Some(d) => (d, self.attr.as_str()),
            None => {
                let attrs: Vec<_> = self.attr.split('.').collect();
                if let Some(&"rosPackages") = attrs.first() {
                    (attrs[1], attrs[2])
                } else {
                    return None;
                }
            }
        };
        Some(format!(
            "https://index.ros.org/p/{}/#{distro}",
            pkg.replace("-", "_")
        ))
    }
    fn ros_index_link(&self, text: &str, eval_distro: Option<&str>) -> String {
        match self.ros_index_url(eval_distro) {
            Some(url) => format!("[{}]({})", text, url),
            None => "".to_owned(),
        }
    }
    fn markdown_link(&self, eval_distro: Option<&str>) -> String {
        match self.ros_index_url(eval_distro) {
            Some(url) => format!("[{}]({})", self.attr, url),
            None => self.attr.to_owned(),
        }
    }
}

impl<'a> HydraAttrStatus<'a> {
    fn compare(&self, other: &HydraAttrStatus) -> CiChange {
        use CiChange::*;
        use HydraAttrStatus::*;
        match (self, other) {
            (Build(_), EvalError(_)) => NewEvalError,
            (EvalError(_), Build(b)) if !b.hydra.success() => FixedEvalErrorBuildFails,
            (EvalError(_), Build(_)) => FixedEvalError,
            (EvalError(_), EvalError(_)) => EvalErrrorNoChange,
            (Build(b1), Build(b2)) if b1.hydra.success() && !b2.hydra.success() => NewBuildFailure,
            (Build(b1), Build(b2)) if !b1.hydra.success() && b2.hydra.success() => {
                FixedBuildFailure
            }
            (Build(b1), Build(b2)) if !b1.hydra.success() && !b2.hydra.success() => {
                BuildFailureNoChange
            }
            (Build(_), Build(_)) => BuildSuccessNoChange,
            (Unbuilt, Unbuilt) => UnbuiltNoChange,
            (Unbuilt, EvalError(_)) => UnbuiltToEvalError,
            (Unbuilt, Build(b)) if b.hydra.success() => UnbuiltToBuildOk,
            (Unbuilt, Build(_)) => UnbuiltToBuildFailure,
            (_, Unbuilt) => NewUnbuiltAttr,
        }
    }
    fn ci_chage_as_new(&self) -> CiChange {
        use CiChange::*;
        use HydraAttrStatus::*;
        match self {
            EvalError(_) => AddedEvalError,
            Build(b) if b.hydra.success() => AddedOk,
            Build(_) => AddedBuildFailure,
            Unbuilt => AddedUnbuilt,
        }
    }
    fn panic_if_aborted(&self, attr: &str) {
        if let HydraAttrStatus::Build(b) = self
            && b.hydra.aborted()
        {
            panic!("attribute {attr} aborted in, see {}", b.hydra.url());
        }
    }
}

struct HydraEvalSummary<'a> {
    eval_id: u64,
    distro: Option<&'a str>,
    attrs: HashMap<&'a str, HydraAttrStatus<'a>>,
}

fn _escape_markdown(input: &str) -> String {
    let special_chars = r"\`*_{}[]()#+-.!";
    input
        .chars()
        .map(|c| {
            if special_chars.contains(c) {
                format!(r"\{}", c)
            } else {
                c.to_string()
            }
        })
        .collect()
}

impl<'a> HydraEvalSummary<'a> {
    fn compare(&self, other: &HydraEvalSummary) {
        let mut summary: HashMap<CiChange, Vec<AttrInfo>> = HashMap::new();
        for (&attr, status) in &self.attrs {
            status.panic_if_aborted(attr);
            let other_status = other.attrs.get(attr);
            let change = match (status, other_status) {
                (_, None) => CiChange::Removed,
                (_, Some(other)) => status.compare(other),
            };
            summary.entry(change).or_default().push(AttrInfo {
                attr: attr.to_string(),
                status: other_status.cloned(),
            });
        }
        for (&attr, other_status) in &other.attrs {
            other_status.panic_if_aborted(attr);
            let self_status = self.attrs.get(attr);
            if self_status.is_none() {
                summary
                    .entry(other_status.ci_chage_as_new())
                    .or_default()
                    .push(AttrInfo {
                        attr: attr.to_string(),
                        status: Some(other_status.clone()),
                    });
            }
        }
        println!("### Hydra build and evaluation statistics");
        for change in CiChange::iter() {
            let list_attrs = change.get_bool("list_attrs").unwrap_or(true);
            let print_summary = change.get_bool("summary").unwrap_or(false);
            summary.entry(change).and_modify(|attrs| {
                let count = attrs.len();
                attrs.sort_by(|a, b| a.attr.cmp(&b.attr));
                println!(
                    "<details>\n\
                          <summary>{change}: {count}</summary>\n\
                          \n"
                );
                if list_attrs || print_summary {
                    let header = OnceCell::new();
                    let mut eval_summary: HashMap<String, Vec<String>> = HashMap::new();
                    for attr_info in attrs {
                        match &attr_info.status {
                            Some(HydraAttrStatus::Build(b)) => {
                                header.get_or_init(|| {
                                    println!("  | Attribute | ROS | deps. | all |");
                                    println!("  |-----------|-----|-------|-----|");
                                });
                                println!(
                                    "  | [{}]({}) | {} | {} | {} |",
                                    attr_info.attr,
                                    b.hydra.url(),
                                    attr_info.ros_index_link("index", self.distro),
                                    b.eval.direct_deps,
                                    b.eval.all_deps
                                );
                            }
                            Some(HydraAttrStatus::EvalError(err)) => {
                                let eval_err_desc = EVAL_ERROR_ANALYZER
                                    .analyze(err)
                                    .map(|reason| format!("``` {} ```", reason))
                                    .unwrap_or("Unrecognized eval error".into());
                                if !print_summary {
                                    header.get_or_init(|| {
                                        println!("  | Attribute | Reason |");
                                        println!("  |-----------|--------|");
                                    });
                                    println!(
                                        "  | {} | {} |",
                                        attr_info.markdown_link(self.distro),
                                        eval_err_desc
                                    )
                                } else {
                                    header.get_or_init(|| {
                                        println!("  | Reason | Attributes |");
                                        println!("  |--------|------------|");
                                    });
                                    eval_summary
                                        .entry(eval_err_desc)
                                        .or_default()
                                        .push(attr_info.markdown_link(self.distro));
                                }
                            }
                            _ => println!("  - {}", attr_info.attr),
                        }
                    }
                    if print_summary {
                        for (reason, attrs) in eval_summary.iter().sorted_by_key(|x| x.0) {
                            println!("  | {reason} | {} |", attrs.join(", "))
                        }
                    }
                } else {
                    println!(
                        "[Hydra comparison](https://hydra.iid.ciirc.cvut.cz/eval/{}?compare={})",
                        other.eval_id, self.eval_id
                    );
                }
                println!("</details>\n");
            });
        }
    }
}

// TODO rename or refactor
#[derive(Serialize, Debug)]
struct Job<'a> {
    job: &'a str,
    direct_deps: usize,
    all_deps: usize,
    build_url: String,
}

type DrvPath = str;
type JobDeps<'a> = HashMap<&'a DrvPath, Vec<&'a DrvPath>>;

impl HydraEval {
    fn summary(&'_ self) -> HydraEvalSummary<'_> {
        let builds: HashMap<&str, HydraBuild> = self
            .hydra_builds
            .iter()
            .map(|build| {
                let job = build["job"].as_str().unwrap();
                (
                    job,
                    serde_json::from_value(build.clone()).unwrap_or_else(|e| {
                        panic!(
                            "Cannot create HydraBuild for {job} (build = {})\nError: {e}",
                            serde_json::to_string_pretty(build).unwrap()
                        )
                    }),
                )
            })
            .collect();
        let job_deps = self.get_eval_job_deps();
        HydraEvalSummary {
            eval_id: self.eval_id,
            distro: self.distro.as_deref(),
            attrs: self
                .eval_jobs
                .iter()
                .map(|job| {
                    let attr = job["attr"].as_str().unwrap();
                    job["error"]
                        .as_str()
                        .map(|err| (attr, HydraAttrStatus::EvalError(err)))
                        .or_else(|| {
                            builds.get(format!("{attr}").as_str()).map(|build| {
                                let cnts = if !build.success() {
                                    // Calculate closure size only for failed build (if we want
                                    // for all, we should optimize the implementation and
                                    // perhaps calculate it in get_eval_job_deps())
                                    dependent_job_counts(&build.drvpath, &job_deps)
                                } else {
                                    vec![]
                                };
                                let direct_deps = *cnts.first().unwrap_or(&0);
                                let all_deps = *cnts.last().unwrap_or(&0);
                                (
                                    attr,
                                    HydraAttrStatus::Build(BuildInfo {
                                        eval: EvalInfo {
                                            direct_deps,
                                            all_deps,
                                        },
                                        hydra: build.clone(),
                                    }),
                                )
                            })
                        })
                        .unwrap_or((attr, HydraAttrStatus::Unbuilt))
                })
                .collect(),
        }
    }

    fn get_eval_job_deps(&self) -> JobDeps<'_> {
        let mut job_deps = JobDeps::new();
        for job in &self.eval_jobs {
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
        job_deps
    }

    fn get_failed_build_stats(&self) -> Vec<Job<'_>> {
        let job_deps = self.get_eval_job_deps();
        let mut failed_jobs = Vec::new();
        for b in self.hydra_builds.iter().filter(|build| {
            build["buildstatus"].as_i64().unwrap_or(
                0, /* queued builds (value null) are not considered failed */
            ) != 0
        }) {
            let cnts = dependent_job_counts(b["drvpath"].as_str().unwrap(), &job_deps);
            failed_jobs.push(Job {
                job: b["job"].as_str().unwrap(),
                direct_deps: *cnts.first().unwrap_or(&0),
                all_deps: *cnts.last().unwrap_or(&0),
                build_url: format!("{HYDRA_URL}/build/{}", b["id"]),
            });
        }
        failed_jobs
    }

    fn compare(&self, other: &Self) {
        self.summary().compare(&other.summary())
    }
}

async fn fetch_hydra_eval(
    hydra: Arc<Hydra>,
    eval_id: u64,
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

    let get_input_url = |input: &str| {
        let url = eval["jobsetevalinputs"][input]["uri"]
            .as_str()
            .expect(format!("No {input}.uri in eval").as_str());
        let rev = eval["jobsetevalinputs"][input]["revision"]
            .as_str()
            .expect(format!("No {input}.revision in eval").as_str());
        format!("{url}/archive/{rev}.tar.gz")
    };

    let release_nix_tarball_url = get_input_url("nix-ros-hydra");
    let tarball_url = get_input_url("nix-ros-overlay");
    let system = eval["jobsetevalinputs"]["system"]["value"]
        .as_str()
        .unwrap();
    let distro = eval["jobsetevalinputs"]["distro"]["value"].as_str();
    let cross_system = eval["jobsetevalinputs"]["crossSystem"]["value"].as_str();

    mp.println(format!("Tarball for evaluation: {tarball_url}"))?;
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
        nix_eval_jobs(
            &release_nix_tarball_url,
            &tarball_url,
            system,
            distro,
            cross_system,
            &pb
        )
    );
    mp.remove(&pb);

    let hydra_builds = hydra_builds
        .into_iter()
        .collect::<anyhow::Result<Vec<_>>>()?;

    Ok(HydraEval {
        eval_id,
        distro: distro.map(str::to_owned),
        hydra_builds,
        eval_jobs: jobs?,
    })
}

fn process_and_print_eval_stats(hydra_eval: HydraEval, cli: cli::EvalArgs) -> anyhow::Result<()> {
    let mut failed_jobs = hydra_eval.get_failed_build_stats();

    failed_jobs.sort_by_key(|job| job.all_deps);

    for fj in &failed_jobs {
        println!("{}", serde_json::to_string(fj)?);
    }

    if cli.eval_failures {
        print_eval_failure_summary(&hydra_eval.eval_jobs);
    }
    Ok(())
}

async fn get_jobset_evals(
    hydra: Arc<Hydra>,
    jobset: &str,
    use_cache: bool,
) -> anyhow::Result<JsonValue> {
    let mut errors = vec![];
    for project in vec!["nix-ros-experiments", "nix-ros-overlay"] {
        match hydra
            .get_with_cachectrl(&format!("jobset/{project}/{jobset}/evals"), use_cache)
            .await
        {
            Ok(evals) => return Ok(evals),
            Err(e) => errors.push(e),
        }
    }
    bail!(
        "Cannot fetch {jobset} jobset because of the following errors:\n  {}",
        errors.iter().map(|e| e.to_string()).join("\n  ")
    );
}

async fn get_latest_jobset_eval(
    hydra: Arc<Hydra>,
    jobset: &str,
    use_cache: bool,
) -> anyhow::Result<u64> {
    let develop_evals = get_jobset_evals(hydra, jobset, use_cache).await?;
    Ok(develop_evals["evals"]
        .as_array()
        .unwrap()
        .first()
        .unwrap()
        .get("id")
        .unwrap()
        .as_u64()
        .unwrap())
}

async fn handle_pr(hydra: Arc<Hydra>, pr_num: usize, mp: &MultiProgress) -> anyhow::Result<()> {
    let gh = Command::new("gh")
        .arg("api")
        .arg(format!("repos/lopsided98/nix-ros-overlay/pulls/{pr_num}"))
        .output()
        .await?;
    let pr: JsonValue = serde_json::from_str(str::from_utf8(&gh.stdout)?)?;
    // println!("{}", &pr);
    let base_sha = pr["base"]["sha"].as_str().unwrap();
    let head_sha = pr["head"]["sha"].as_str().unwrap();

    // Try with the cache first, then without
    let mut use_cache = true;
    let (base_eval, head_eval) = loop {
        let jobsets = join_all(vec![
            hydra.get_with_cachectrl(
                "jobset/nix-ros-experiments/wentasah-rosdistro-sync/evals",
                use_cache,
            ),
            hydra.get_with_cachectrl("jobset/nix-ros-experiments/wentasah-test/evals", use_cache),
            hydra.get_with_cachectrl(
                "jobset/nix-ros-experiments/lopsided98-develop/evals",
                use_cache,
            ),
        ])
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;

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
        match (base_eval, head_eval, use_cache) {
            (Some(be), Some(he), _) => break (be, he),
            (_, _, true) => {
                // Second try without cache
                use_cache = false;
                continue;
            }
            // No eval found for develop branch - use the lastest
            // eval. It can happen when the last commit doesn't change
            // the result of evaluation, e.g. it changes README or CI.
            (None, Some(he), false)
                if pr["base"]["repo"]["full_name"] == "lopsided98/nix-ros-overlay"
                    && pr["base"]["ref"] == "develop" =>
            {
                let develop_latest_eval =
                    get_latest_jobset_eval(hydra.clone(), "lopsided98-develop", false).await?;
                warn!(
                    "Cannot find Hydra evaluation for base commit {base_sha}. \
                     Using latest evaluation {develop_latest_eval} of the develop branch instead."
                );
                break (develop_latest_eval, he);
            }
            (_, _, _) => bail!(
                "Cannot find needed Hydra evaluations for both commits: base={base_sha}->{} head={head_sha}->{}",
                base_eval.map_or("???".to_string(), |v| v.to_string()),
                head_eval.map_or("???".to_string(), |v| v.to_string()),
            ),
        }
    };

    compare_evals(hydra, mp, base_eval, head_eval).await
}

async fn compare_evals(
    hydra: Arc<Hydra>,
    mp: &MultiProgress,
    base_eval: u64,
    head_eval: u64,
) -> anyhow::Result<()> {
    let evals = join_all(vec![
        fetch_hydra_eval(hydra.clone(), base_eval, mp),
        fetch_hydra_eval(hydra.clone(), head_eval, mp),
    ])
    .await
    .into_iter()
    .collect::<Result<Vec<_>, _>>()?;
    evals[0].compare(&evals[1]);
    Ok(())
}

async fn compare_jobsets(
    hydra: Arc<Hydra>,
    mp: &MultiProgress,
    old_jobset: &str,
    new_jobset: &str,
    use_cache: bool,
) -> anyhow::Result<()> {
    let evals = join_all(vec![
        fetch_hydra_eval(
            hydra.clone(),
            get_latest_jobset_eval(hydra.clone(), old_jobset, use_cache).await?,
            mp,
        ),
        fetch_hydra_eval(
            hydra.clone(),
            get_latest_jobset_eval(hydra.clone(), new_jobset, use_cache).await?,
            mp,
        ),
    ])
    .await
    .into_iter()
    .collect::<Result<Vec<_>, _>>()?;
    evals[0].compare(&evals[1]);
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::builder()
        .format_timestamp(None)
        .filter_level(log::LevelFilter::Info)
        .init();
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
        cli::Commands::CompareEvals { old, new } => {
            compare_evals(hydra.clone(), &mp, old, new).await?
        }
        cli::Commands::CompareJobsets {
            old,
            new,
            use_cache,
        } => compare_jobsets(hydra.clone(), &mp, &old, &new, use_cache).await?,
    };
    mp.clear()?;

    Ok(())
}
