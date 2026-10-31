use anyhow::{bail, Context, Result};
use clap::Parser;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::HashMap, fs::{self, OpenOptions}, io::Write, path::PathBuf, thread, time::{Duration, SystemTime, UNIX_EPOCH}};

#[derive(Parser)]
#[command(version, about = "Read-only memecoin pair monitoring")]
struct Args {
    #[arg(short, long, default_value = "config.toml")]
    config: PathBuf,
    /// Fetch one cycle and exit
    #[arg(long)]
    once: bool,
}
#[derive(Deserialize)]
struct Config {
    poll_seconds: u64,
    cooldown_seconds: u64,
    history_file: PathBuf,
    watchlist: Vec<Token>,
}
#[derive(Deserialize)]
struct Token {
    label: String,
    chain: String,
    pair: String,
    above_usd: Option<f64>,
    below_usd: Option<f64>,
    change_percent: Option<f64>,
    /// DEX Screener rolling window: m5, h1, h6, h24
    window: String,
    min_volume_h24: Option<f64>,
}
#[derive(Serialize)]
struct Event<'a> {
    timestamp: u64,
    label: &'a str,
    chain: &'a str,
    pair: &'a str,
    price_usd: f64,
    quote: &'a str,
    volume_h24: Option<f64>,
    reason: &'a str,
}
fn number(v: &Value) -> Option<f64> {
    v.as_f64().or_else(|| v.as_str()?.parse().ok()).filter(|x| x.is_finite())
}
fn reasons(t: &Token, price: f64, change: Option<f64>, volume: Option<f64>) -> Vec<&'static str> {
    let mut out = Vec::new();
    if t.above_usd.is_some_and(|x| price >= x) { out.push("above_price"); }
    if t.below_usd.is_some_and(|x| price <= x) { out.push("below_price"); }
    if t.change_percent.zip(change).is_some_and(|(x,c)| if x >= 0.0 { c >= x } else { c <= x }) { out.push("percentage_change"); }
    if t.min_volume_h24.zip(volume).is_some_and(|(x,v)| v >= x) { out.push("volume_h24"); }
    out
}
fn main() -> Result<()> {
    let args = Args::parse();
    let cfg: Config = toml::from_str(&fs::read_to_string(&args.config).context("read config (copy config.example.toml first)")?)?;
    if cfg.poll_seconds < 10 || cfg.watchlist.is_empty() || cfg.watchlist.len() > 40 { bail!("poll_seconds must be >=10; watchlist must contain 1..40 pairs"); }
    for t in &cfg.watchlist {
        if !["m5","h1","h6","h24"].contains(&t.window.as_str()) { bail!("invalid window for {}",t.label); }
        for s in [&t.chain,&t.pair] { if s.is_empty() || !s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') { bail!("invalid chain/pair"); } }
        for x in [t.above_usd,t.below_usd,t.min_volume_h24].into_iter().flatten() { if !x.is_finite() || x <= 0.0 { bail!("thresholds must be finite and positive"); } }
        if t.change_percent.is_some_and(|x| !x.is_finite() || x == 0.0) { bail!("change_percent must be finite and nonzero"); }
        if t.below_usd.zip(t.above_usd).is_some_and(|(l,h)| l >= h) { bail!("below_usd must be less than above_usd"); }
    }
    let client = reqwest::blocking::Client::builder().timeout(Duration::from_secs(20)).user_agent("memecoin-price-alert-bot/0.1").build()?;
    let mut last = HashMap::<(usize,String),u64>::new();
    let mut history = OpenOptions::new().create(true).append(true).open(&cfg.history_file)?;
    loop {
        let mut failures = 0;
        for (index,t) in cfg.watchlist.iter().enumerate() {
            let result = (|| -> Result<()> {
                let data: Value = client.get(format!("https://api.dexscreener.com/latest/dex/pairs/{}/{}",t.chain,t.pair)).send()?.error_for_status()?.json()?;
                let p = data["pairs"].as_array().and_then(|ps| ps.iter().find(|p| p["chainId"].as_str() == Some(&t.chain) && p["pairAddress"].as_str() == Some(&t.pair))).context("pair unavailable")?;
                let price = number(&p["priceUsd"]).filter(|x| *x > 0.0).context("USD price unavailable")?;
                let volume = number(&p["volume"]["h24"]);
                println!("{}: ${:.10} | {} / {} | volume24h {:?}",t.label,price,p["baseToken"]["symbol"],p["quoteToken"]["symbol"],volume);
                let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
                for reason in reasons(t,price,number(&p["priceChange"][&t.window]),volume) {
                    let key = (index,reason.to_string());
                    if last.get(&key).is_some_and(|ts| now.saturating_sub(*ts) < cfg.cooldown_seconds) { continue; }
                    let event = Event { timestamp: now,label: &t.label,chain: &t.chain,pair: &t.pair,price_usd: price,quote: p["quoteToken"]["symbol"].as_str().unwrap_or("unknown"),volume_h24: volume,reason };
                    writeln!(history,"{}",serde_json::to_string(&event)?)?;
                    history.flush()?;
                    println!("ALERT {}: {}",t.label,reason);
                    last.insert(key,now);
                }
                Ok(())
            })();
            if let Err(e) = result { failures += 1; eprintln!("{}: {e:#}",t.label); }
            thread::sleep(Duration::from_millis(250));
        }
        if args.once { if failures > 0 { bail!("{failures} watchlist entries failed"); } return Ok(()); }
        thread::sleep(Duration::from_secs(cfg.poll_seconds));
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn thresholds_and_missing_data() {
        let t = Token { label: "test".into(),chain: "solana".into(),pair: "abc".into(),above_usd: Some(2.0),below_usd: Some(1.0),change_percent: Some(-5.0),window: "h1".into(),min_volume_h24: Some(100.0) };
        assert_eq!(reasons(&t,1.0,Some(-5.0),Some(100.0)),vec!["below_price","percentage_change","volume_h24"]);
        assert!(reasons(&t,1.5,None,None).is_empty());
        assert_eq!(number(&serde_json::json!("0.001")),Some(0.001));
        assert_eq!(number(&serde_json::json!("NaN")),None);
    }
}
