use anyhow::{anyhow, Result};
use clap::Parser;
use log::{error, info};
use reqwest::{
    header::{HeaderMap, HeaderValue, USER_AGENT},
    StatusCode,
};
use serde_derive::{Deserialize, Serialize};
use std::{
    ffi::{OsStr, OsString},
    fs::File,
    io,
    net::Ipv4Addr,
    path::Path,
    time::Duration,
};
use tempfile::NamedTempFile;
use tokio::time::{self, MissedTickBehavior};

/// A simple Namecheap Dynamic DNS client.
#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// The config file to use (read-only).
    #[arg(long, value_name = "FILE")]
    config: OsString,

    /// The state file to use (read/write).
    #[arg(long, value_name = "FILE")]
    state: OsString,
}

/// Config (read-only).
#[derive(Deserialize)]
struct Config {
    /// The domain to update.
    domain: String,

    /// The host (aka subdomain) to set DNS for. Omit, or specify `@`, to update the bare domain.
    /// Specify `*` to update the wildcard subdomain.
    host: Option<String>,

    /// The dynamic DNS password.
    password: String,
}

// State (read/write).
#[derive(Default, Serialize, Deserialize)]
struct State {
    /// Our current conception of what Namecheap thinks our IP address is.
    addr: Option<Ipv4Addr>,
}

#[tokio::main]
async fn main() {
    simple_logger::init_with_env().unwrap();
    let args = Args::parse();

    // Parse config & state files.
    let cfg: Config = {
        let config_file = File::open(args.config).expect("Couldn't open config file");
        serde_yaml::from_reader(config_file).expect("Couldn't parse config file")
    };
    let mut state: State = match File::open(&args.state) {
        Ok(state_file) => serde_yaml::from_reader(state_file).expect("Couldn't parse state file"),
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            let state = State::default();
            update_state(&args.state, &state)
                .await
                .expect("Couldn't write initial state file");
            state
        }
        Err(err) => panic!("Couldn't read state file: {}", err),
    };

    // Create an HTTP client.
    let client = reqwest::Client::builder()
        .default_headers(HeaderMap::from_iter([(
            USER_AGENT,
            HeaderValue::from_str(&format!("rnccd {}", env!("CARGO_PKG_VERSION")))
                .expect("Couldn't create default HTTP headers"),
        )]))
        .timeout(Duration::from_secs(30))
        .build()
        .expect("Couldn't create HTTP client");

    // Main loop: check IP every now and then, update if necessary.
    info!("Starting: will check & update IP every 60s");
    let mut interval = time::interval(Duration::from_secs(60));
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut namecheap_addr = state.addr; // namecheap_addr stores our belief about what Namecheap thinks our IP is.
    loop {
        interval.tick().await;

        // Figure out what our current IP is.
        let current_addr = match current_address(&client).await {
            Ok(addr) => addr,
            Err(e) => {
                error!("Couldn't get current IP address: {}", e);
                continue;
            }
        };

        // Update IP in Namecheap if it differs.
        if Some(current_addr) != namecheap_addr {
            info!(
                "Detected new IP ({} -> {}), updating",
                fmt_optional_addr(&namecheap_addr),
                current_addr
            );
            if let Err(err) = update_address(&client, &cfg, current_addr).await {
                error!("Couldn't update IP address: {}", err);
                continue;
            }
            namecheap_addr = Some(current_addr);
        }

        // Update state on disk if it differs.
        if Some(current_addr) != state.addr {
            let new_state = State {
                addr: Some(current_addr),
            };
            if let Err(err) = update_state(&args.state, &new_state).await {
                error!("Couldn't write state file: {}", err);
                continue;
            }
            state = new_state;
        }
    }
}

fn fmt_optional_addr(addr: &Option<Ipv4Addr>) -> String {
    match addr {
        None => "None".into(),
        Some(a) => a.to_string(),
    }
}

async fn current_address(client: &reqwest::Client) -> Result<Ipv4Addr> {
    let resp = client.get("https://api.ipify.org").send().await?;
    if resp.status() != StatusCode::OK {
        return Err(anyhow!("unexpected status code: {}", resp.status()));
    }
    Ok(resp.text().await?.parse()?)
}

async fn update_address(client: &reqwest::Client, cfg: &Config, addr: Ipv4Addr) -> Result<()> {
    let resp = client
        .get("https://dynamicdns.park-your-domain.com/update")
        .query(&[
            ("host", cfg.host.as_deref().unwrap_or("@")),
            ("domain", &cfg.domain),
            ("password", &cfg.password),
            ("ip", &addr.to_string()),
        ])
        .send()
        .await?;

    // This API always returns 200 OK, and communicates errors via an unschema'ed XML document in
    // the body. I don't want to depend on an entire XML parser, so look for an error count of 0 to
    // communicate success.
    let body = resp.text().await?;
    if body.contains("<ErrCount>0</ErrCount>") {
        return Ok(());
    }
    Err(anyhow!("update request got error: {}", body))
}

async fn update_state(state_path: &OsStr, state: &State) -> Result<()> {
    let state_path = Path::new(state_path);
    let dir = state_path.parent().ok_or_else(|| {
        anyhow!(
            "couldn't determine parent directory of {}",
            state_path.display()
        )
    })?;
    let temp_file = NamedTempFile::new_in(dir)?;
    serde_yaml::to_writer(&temp_file, state)?;
    temp_file.persist(state_path)?;
    Ok(())
}
