mod config;
mod extract;
mod index;
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
                "indexed {} notes, {} projects, {} entities, {} edges",
                stats.notes, stats.projects, stats.entities, stats.edges
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
