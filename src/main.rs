mod config;
mod extract;
mod index;
mod mcp;
mod query;
mod scan;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "raft",
    about = "A personal knowledge graph grown from notes and repos",
    version
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Write a default config if none exists and show its location
    Init,
    /// Scan all configured sources and rebuild the index
    Index,
    /// Full-text search across notes
    Search {
        term: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Everything the graph knows about a project or entity
    About {
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Open loops (follow-ups, unchecked boxes), stalest first
    Dangling {
        /// Only loops mentioning this project or entity
        #[arg(long)]
        about: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Connections nobody wrote down: co-mention affinity and
    /// projects whose commits travel together
    Connect {
        /// Minimum shared notes / shared commit days for a pair
        #[arg(long, default_value_t = 3)]
        min: i64,
        #[arg(long, default_value_t = 15)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Serve the graph to agents as an MCP server over stdio
    Serve,
    /// List configured sources
    Sources,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Init => {
            let path = config::Config::write_default()?;
            println!("config: {}", path.display());
            println!("db:     {}", config::db_path()?.display());
        }
        Command::Index => {
            let cfg = config::Config::load()?;
            let stats = index::rebuild(&cfg)?;
            println!(
                "indexed {} notes, {} projects, {} entities, {} loops, {} edges",
                stats.notes, stats.projects, stats.entities, stats.loops, stats.edges
            );
        }
        Command::Search { term, limit, json } => {
            let conn = index::open_db()?;
            let hits = query::search(&conn, &term, limit)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&hits)?);
            } else if hits.is_empty() {
                println!("no matches for '{term}'");
            } else {
                for hit in hits {
                    let date = hit.note_date.as_deref().unwrap_or("          ");
                    println!("{}  {}", date, hit.path);
                    if !hit.snippet.is_empty() {
                        println!("    ...{}...", hit.snippet);
                    }
                }
            }
        }
        Command::About { name, json } => {
            let conn = index::open_db()?;
            match query::about(&conn, &name)? {
                None => println!("nothing known about '{name}' (try `raft index` first)"),
                Some(about) if json => println!("{}", serde_json::to_string_pretty(&about)?),
                Some(about) => print_about(&about),
            }
        }
        Command::Dangling { about, limit, json } => {
            let conn = index::open_db()?;
            let loops = query::dangling(&conn, about.as_deref(), limit)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&loops)?);
            } else if loops.is_empty() {
                println!("no open loops found");
            } else {
                for l in &loops {
                    let age = l
                        .age_days
                        .map(|d| format!("{d}d"))
                        .unwrap_or_else(|| "?".into());
                    let text: String = if l.text.chars().count() > 110 {
                        let truncated: String = l.text.chars().take(107).collect();
                        format!("{truncated}...")
                    } else {
                        l.text.clone()
                    };
                    println!("{age:>5}  {text}");
                    let seen = if l.sightings > 1 {
                        format!("  (seen {}x)", l.sightings)
                    } else {
                        String::new()
                    };
                    println!(
                        "       {}  {}{}",
                        l.first_seen.as_deref().unwrap_or(""),
                        l.note_path,
                        seen
                    );
                }
                println!("\n{} open loops", loops.len());
            }
        }
        Command::Connect { min, limit, json } => {
            let conn = index::open_db()?;
            let connections = query::connect(&conn, min, limit)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&connections)?);
            } else {
                if !connections.co_mentions.is_empty() {
                    println!("keep showing up together:");
                    for p in &connections.co_mentions {
                        println!(
                            "  {:.2}  {} ({}) <-> {} ({})  [{} notes over {} days]",
                            p.score, p.a, p.a_kind, p.b, p.b_kind, p.shared_notes, p.span_days
                        );
                    }
                }
                if !connections.temporal.is_empty() {
                    println!("\ncommits travel together:");
                    for p in &connections.temporal {
                        println!("  {:2} days  {} <-> {}", p.shared_days, p.a, p.b);
                    }
                }
                if connections.co_mentions.is_empty() && connections.temporal.is_empty() {
                    println!("no connections above the threshold (try --min 2)");
                }
            }
        }
        Command::Serve => {
            mcp::serve()?;
        }
        Command::Sources => {
            let cfg = config::Config::load()?;
            for source in &cfg.sources {
                println!(
                    "{:9} {}",
                    format!("{:?}", source.kind).to_lowercase(),
                    source.path
                );
            }
        }
    }

    Ok(())
}

fn print_about(about: &query::About) {
    println!("{} ({})", about.name, about.kind);

    if let Some(git) = &about.git {
        if let Some(branch) = git.get("branch").and_then(|b| b.as_str()) {
            println!("\n  branch: {branch}");
        }
        if let Some(commits) = git.get("commits").and_then(|c| c.as_array()) {
            for commit in commits.iter().take(5) {
                let date = commit.get("date").and_then(|v| v.as_str()).unwrap_or("");
                let subject = commit.get("subject").and_then(|v| v.as_str()).unwrap_or("");
                println!("  {date}  {subject}");
            }
        }
    }

    if !about.notes.is_empty() {
        println!("\nnotes ({}):", about.notes.len());
        for note in about.notes.iter().take(15) {
            let date = note.note_date.as_deref().unwrap_or("          ");
            println!("  {}  {}", date, note.path);
        }
    }

    if !about.co_mentioned.is_empty() {
        println!("\nco-mentioned:");
        for co in &about.co_mentioned {
            println!("  {:3}x  {} ({})", co.shared_notes, co.name, co.kind);
        }
    }
}
