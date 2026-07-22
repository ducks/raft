mod capture;
mod config;
mod extract;
mod graph;
mod index;
mod mcp;
mod publish;
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
    /// Why a project or entity is in the graph: every edge pointing at it,
    /// with provenance, confidence, and the evidence that created it
    Why {
        name: String,
        /// Hide edges below this confidence weight (backticked-span
        /// mentions score 0.3; wiki links and dictionary hits score higher)
        #[arg(long, default_value_t = 0.0)]
        min_weight: f64,
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
        /// Hide edges below this confidence weight before pairing.
        /// The default excludes weak backticked-span guesses (weight 0.3);
        /// use --min-weight 0 to include everything
        #[arg(long, default_value_t = 0.5)]
        min_weight: f64,
        #[arg(long, default_value_t = 15)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Render the neighborhood around a node as a graph (DOT or Mermaid).
    /// Pipe DOT to `dot -Tsvg` or paste Mermaid into any renderer.
    Graph {
        /// Project or entity to center the graph on
        name: String,
        /// How many hops out from the start node to include
        #[arg(long, default_value_t = 2)]
        depth: usize,
        /// Hide edges below this confidence weight (weak backticked-span
        /// guesses score 0.3); use 0 to include everything
        #[arg(long, default_value_t = 0.5)]
        min_weight: f64,
        /// Output format: dot (Graphviz) or mermaid
        #[arg(long, default_value = "dot")]
        format: String,
    },
    /// Append a timestamped entry to today's daily note and reindex
    Log {
        /// The entry text (multiple words fine, no quotes needed)
        #[arg(required = true)]
        text: Vec<String>,
    },
    /// Mark an open loop done in its source note(s) and reindex
    Done {
        /// Substring of the loop's text (must match exactly one loop)
        pattern: String,
    },
    /// Compute what would be published (notes opted in via
    /// `publish: true` frontmatter, repos allowlisted in config).
    /// Emitting the site is not implemented yet; only --audit works.
    Publish {
        /// Print the manifest of everything that would go public
        #[arg(long)]
        audit: bool,
        #[arg(long)]
        json: bool,
    },
    /// Serve the graph to agents as an MCP server over stdio
    Serve,
    /// List configured sources
    Sources,
    /// Show index health, freshness, graph counts, and source accessibility
    Status {
        #[arg(long)]
        json: bool,
    },
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
            if stats.git_cached > 0 || stats.git_refreshed > 0 {
                println!(
                    "git: {} refreshed, {} reused from cache",
                    stats.git_refreshed, stats.git_cached
                );
            }
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
        Command::Why {
            name,
            min_weight,
            json,
        } => {
            let conn = index::open_db()?;
            match query::why(&conn, &name, min_weight)? {
                None => println!("nothing known about '{name}' (try `raft index` first)"),
                Some(facts) if json => println!("{}", serde_json::to_string_pretty(&facts)?),
                Some(facts) if facts.is_empty() => {
                    println!("no edges point at '{name}' above weight {min_weight}")
                }
                Some(facts) => {
                    println!("{} ({} edges)", name, facts.len());
                    for f in &facts {
                        let src = shorten_source(&f.from, &f.from_kind);
                        println!(
                            "  {:.2}  {:<8} {:<9} {}",
                            f.weight, f.provenance, f.relation, src
                        );
                        if let Some(r) = &f.rationale {
                            println!("           {r}");
                        }
                    }
                }
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
        Command::Connect {
            min,
            min_weight,
            limit,
            json,
        } => {
            let conn = index::open_db()?;
            let connections = query::connect(&conn, min, min_weight, limit)?;
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
        Command::Graph {
            name,
            depth,
            min_weight,
            format,
        } => {
            let Some(fmt) = graph::GraphFormat::parse(&format) else {
                eprintln!("unknown format '{format}' (expected: dot, mermaid)");
                std::process::exit(2);
            };
            let conn = index::open_db()?;
            match graph::neighborhood(&conn, &name, depth, min_weight)? {
                None => println!("nothing known about '{name}' (try `raft index` first)"),
                Some(sub) => {
                    let out = match fmt {
                        graph::GraphFormat::Dot => graph::to_dot(&sub),
                        graph::GraphFormat::Mermaid => graph::to_mermaid(&sub),
                    };
                    print!("{out}");
                }
            }
        }
        Command::Log { text } => {
            let cfg = config::Config::load()?;
            let entry = text.join(" ");
            let path = capture::append_log(&cfg, &entry)?;
            index::rebuild(&cfg)?;
            println!("logged to {}", path.display());
        }
        Command::Done { pattern } => {
            let cfg = config::Config::load()?;
            let conn = index::open_db()?;
            let matches = query::find_open_loops(&conn, &pattern)?;
            match matches.len() {
                0 => println!("no open loop matches '{pattern}'"),
                1 => {
                    let m = &matches[0];
                    let changed = capture::mark_done(&m.text, std::slice::from_ref(&m.note))?;
                    if changed.is_empty() {
                        println!("loop found in the index but not in the notes; run `raft index`");
                    } else {
                        index::rebuild(&cfg)?;
                        println!("done: {}", m.text);
                        for path in changed {
                            println!("  updated {path}");
                        }
                    }
                }
                n => {
                    println!("'{pattern}' matches {n} loops; be more specific:");
                    for m in matches.iter().take(10) {
                        let text: String = m.text.chars().take(90).collect();
                        println!("  - {text}");
                    }
                }
            }
        }
        Command::Publish { audit, json } => {
            if !audit {
                println!("emit is not implemented yet; run `raft publish --audit`");
                std::process::exit(2);
            }
            let cfg = config::Config::load()?;
            let conn = index::open_db()?;
            let plan = publish::plan(&conn, &cfg.publish)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&plan)?);
            } else {
                print_audit(&plan);
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
        Command::Status { json } => {
            let cfg = config::Config::load()?;
            let status = index::status(&cfg)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                print_status(&status);
            }
        }
    }

    Ok(())
}

fn print_status(status: &index::IndexStatus) {
    let state = if status.healthy {
        "healthy"
    } else if !status.indexed {
        "missing"
    } else {
        "needs attention"
    };
    println!("index:        {state}");
    println!("database:     {}", status.database);
    match status.schema_version {
        Some(version) => println!(
            "schema:       {version} (expected {})",
            status.expected_schema_version
        ),
        None => println!("schema:       -"),
    }
    println!(
        "last rebuilt: {}",
        status.last_rebuilt.as_deref().unwrap_or("never")
    );
    if let Some(counts) = &status.counts {
        println!(
            "graph:        {} notes, {} projects, {} entities, {} loops, {} edges",
            counts.notes, counts.projects, counts.entities, counts.loops, counts.edges
        );
    }
    if let Some(error) = &status.error {
        println!("error:        {error}");
    }
    println!("sources:");
    for source in &status.sources {
        let state = if source.healthy { "ok" } else { "error" };
        println!("  {:9} {:5} {}", source.kind, state, source.path);
        if let Some(error) = &source.error {
            println!("                  {error}");
        }
    }
}

fn print_audit(plan: &publish::PublishPlan) {
    println!(
        "would publish: {} notes, {} projects, {} entities, {} loops, {} edges",
        plan.notes.len(),
        plan.projects.len(),
        plan.entities.len(),
        plan.loops.len(),
        plan.edges.len()
    );
    if !plan.notes.is_empty() {
        println!("\nnotes:");
        for n in &plan.notes {
            let date = n.note_date.as_deref().unwrap_or("          ");
            println!("  {}  {}", date, n.path);
        }
    }
    if !plan.projects.is_empty() {
        println!("\nprojects:");
        for p in &plan.projects {
            println!("  {}", p.name);
        }
    }
    if !plan.entities.is_empty() {
        println!("\nentities:");
        for e in &plan.entities {
            println!("  {}", e.name);
        }
    }
    if !plan.loops.is_empty() {
        println!("\nopen loops:");
        for l in &plan.loops {
            println!("  {}  ({})", l.text, l.note_path);
        }
    }
    if !plan.flags.is_empty() {
        println!("\nFLAGS - need a decision before emit:");
        for f in &plan.flags {
            println!("  {}", f.reason);
            println!("    in {}", f.note_path);
        }
    }
}

/// Note sources are stored by full path; show just the filename. Other
/// kinds (loop, entity) already carry a readable name.
fn shorten_source(name: &str, kind: &str) -> String {
    if kind == "note" {
        std::path::Path::new(name)
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or(name)
            .to_string()
    } else {
        name.to_string()
    }
}

fn print_about(about: &query::About) {
    println!("{} ({})", about.name, about.kind);

    if let Some(def) = &about.definition {
        let file = def.get("file").and_then(|v| v.as_str()).unwrap_or("");
        let repo = def.get("repo").and_then(|v| v.as_str()).unwrap_or("");
        println!("\n  defined in: {repo}/{file}");
    }

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
