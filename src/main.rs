use futures::stream::{self, StreamExt};
use indicatif::ProgressIterator;
use std::{path::Path, sync::Arc};
use tokio::fs;

static HYDRA_URL: &str = "http://10.35.95.5:3000";

struct Hydra {
    client: reqwest::Client,
}

impl Hydra {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }

    async fn get(&self, path: &str) -> anyhow::Result<serde_json::Value> {
        let cache_path = format!(".cache/{path}");
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let eval_id = 6104;

    let hydra = Arc::new(Hydra::new());

    let eval = hydra.get(format!("eval/{eval_id}").as_str()).await?;

    stream::iter(
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
    .await;
    Ok(())
}
