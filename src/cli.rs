use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;

use crate::scan;
use crate::state::AppState;

#[derive(Parser, Debug)]
#[command(version, about = "yt-dlp to STRM bridge server")]
pub struct Cli {
    #[arg(short, long)]
    pub config: Option<PathBuf>,

    /// URL to scan (channel or playlist). Bypasses web server and runs scan immediately.
    #[arg(long)]
    pub scan: Option<String>,

    /// Scan ALL scheduled sources sequentially. Bypasses web server.
    #[arg(long)]
    pub scan_all: bool,

    /// Force rescan (ignores existing images/NFOs). Use with --scan or --scan-all.
    #[arg(long)]
    pub force: bool,

    /// Override the video limit for CLI scans. Defaults to 10000.
    #[arg(long, default_value_t = 10000)]
    pub limit: usize,
}

/// Returns true if a CLI command was executed and the process should exit.
pub async fn handle_scan_commands(state: &AppState, cli: &Cli) -> bool {
    if let Some(url) = &cli.scan {
        tracing::info!(
            "Starting single CLI scan for: {} (force={}, limit={})",
            url,
            cli.force,
            cli.limit
        );
        run_single_scan(state, url, cli.force, cli.limit).await;
        return true;
    }

    if cli.scan_all {
        tracing::info!(
            "Starting FULL CLI scan of all scheduled sources (force={}, limit={})",
            cli.force,
            cli.limit
        );

        let (sources, mode) = {
            let cfg = state.config();
            let sources = cfg
                .schedule
                .sources
                .iter()
                .map(|s| {
                    // For CLI full-scan, honour the larger of: config limit, CLI limit.
                    // Prevents accidentally truncating a large channel while still
                    // allowing --limit=100 for a quick test.
                    (s.url.clone(), s.limit.max(cli.limit))
                })
                .collect::<Vec<_>>();
            (sources, cfg.strm.mode)
        };

        if sources.is_empty() {
            println!("No sources found in config schedule.");
            return true;
        }

        println!("Found {} sources to scan.", sources.len());

        for (i, (url, limit)) in sources.iter().enumerate() {
            println!("\n========================================================");
            println!(
                "Scanning {}/{}: {} (limit={})",
                i + 1,
                sources.len(),
                url,
                limit
            );
            println!("========================================================");

            let result = scan::run_scan(state, url, *limit, cli.force, mode, None).await;

            match result {
                Ok(res) => {
                    println!(
                        "✅ Success: {} | {} STRMs | {} NFOs",
                        res.channel, res.strm_written, res.nfo_written
                    );
                }
                Err(e) => {
                    eprintln!("❌ Failed: {}", e);
                }
            }

            tokio::time::sleep(Duration::from_secs(5)).await;
        }

        println!("\n=== All Scans Complete ===");
        return true;
    }

    false
}

async fn run_single_scan(state: &AppState, url: &str, force: bool, limit: usize) {
    let mode = state.config().strm.mode;
    let started = std::time::Instant::now();

    let result = scan::run_scan(state, url, limit, force, mode, None).await;

    match result {
        Ok(res) => {
            let elapsed = started.elapsed().as_secs();
            println!("\n╭────────────────────────────────────────────────────────╮");
            println!("│ Scan Complete: {:<40} │", truncate(&res.channel, 40));
            println!("├────────────────────────────────────────────────────────┤");
            println!("│ Total Videos Scanned : {:<30} │", res.total);
            println!(
                "│ STRMs                : {:<3} written / {:<3} skipped     │",
                res.strm_written, res.strm_skipped
            );
            println!(
                "│ NFOs                 : {:<3} written / {:<3} skipped     │",
                res.nfo_written, res.nfo_skipped
            );
            println!(
                "│ Thumbnails           : {:<3} written / {:<3} skipped     │",
                res.thumb_written, res.thumb_skipped
            );
            println!(
                "│ Time Elapsed         : {:<30} │",
                format!("{}m {}s", elapsed / 60, elapsed % 60)
            );

            if !res.failures.is_empty() {
                println!("├────────────────────────────────────────────────────────┤");
                println!("│ ⚠️  Failures          : {:<30} │", res.failures.len());
                for f in &res.failures {
                    println!("│   - {:<50} │", truncate(f, 50));
                }
            }
            println!("╰────────────────────────────────────────────────────────╯");
        }
        Err(e) => eprintln!("\n❌ Scan failed: {:#}", e),
    }
}

// Helper to keep the box aligned
fn truncate(s: &str, max_len: usize) -> String {
    if s.len() > max_len {
        format!("{}...", &s[..max_len - 3])
    } else {
        s.to_string()
    }
}
