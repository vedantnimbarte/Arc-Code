//! `wingman context` — what Wingman sends before you type anything.
//!
//! Every agent pays a fixed context tax per turn: a system prompt, a tool
//! schema for each registered tool, project instructions, memories. It is
//! billed on every request and it crowds out the conversation, but it is
//! normally invisible — you find out via the bill.
//!
//! This prints the breakdown and the first-turn total, so the number is
//! checkable rather than claimed. Being able to say "this is what we send, run
//! it yourself" is worth more than an adjective.

use anyhow::Result;
use std::process::ExitCode;
use wingman_config::{Config, ProjectPaths};

pub async fn run(cfg: Config, json: bool) -> Result<ExitCode> {
    let cwd = std::env::current_dir()?;
    let paths = ProjectPaths::discover(&cwd);
    let mode = cfg.permission_mode;

    // Build the same registry a real session would, so the tool list and its
    // schemas are the actual ones — not an approximation.
    // Wrapped and given its deferred-access tools so the count below is the
    // one a session actually pays, including the two meta-tool schemas that
    // `[tools].defer` adds in exchange for the ones it hides.
    let registry = std::sync::Arc::new(crate::runtime::build_registry(&cfg, mode).await?);
    crate::runtime::register_deferred_access(&registry, &cfg);
    let specs = {
        use wingman_core::ToolDispatcher;
        registry.specs()
    };

    let memory_store = wingman_learn::memory::MemoryStore::new(paths.root.clone());
    let skills = wingman_skills::load_all(&paths.root);
    let system = crate::runtime::build_system_prompt_full(mode, &memory_store, &skills);

    let system_tokens = wingman_core::tokens::estimate_tokens(&system);

    let mut tools: Vec<(String, u32, usize)> = specs
        .iter()
        .map(|s| {
            let schema = serde_json::to_string(&s.input_schema).unwrap_or_default();
            let text = format!("{}{}{}", s.name, s.description, schema);
            (
                s.name.clone(),
                wingman_core::tokens::estimate_tokens(&text),
                text.len(),
            )
        })
        .collect();
    // Largest first — that's the actionable end of the list.
    tools.sort_by_key(|(_, tokens, _)| std::cmp::Reverse(*tokens));

    let tool_tokens: u32 = tools.iter().map(|(_, t, _)| t).sum();
    let total = system_tokens + tool_tokens;

    if json {
        let out = serde_json::json!({
            "system_prompt_tokens": system_tokens,
            "system_prompt_bytes": system.len(),
            "tool_count": tools.len(),
            "tool_schema_tokens": tool_tokens,
            "first_turn_tokens": total,
            "tools": tools.iter().map(|(n, t, b)| serde_json::json!({
                "name": n, "tokens": t, "bytes": b
            })).collect::<Vec<_>>(),
        });
        println!("{}", serde_json::to_string_pretty(&out)?);
        return Ok(ExitCode::SUCCESS);
    }

    println!(
        "wingman context — {} (mode: {mode})\n",
        paths.root.display()
    );
    println!(
        "  system prompt   {system_tokens:>7} tokens  ({} bytes)",
        system.len()
    );
    println!(
        "  tool schemas    {tool_tokens:>7} tokens  ({} tools)",
        tools.len()
    );
    println!("  {:-<44}", "");
    println!("  first turn      {total:>7} tokens  before your prompt");

    // Cost per turn, when we can price the model. This is the number that
    // actually motivates trimming.
    if let Some(model) = &cfg.default_model {
        if let Some(price) = wingman_core::pricing::price_for(model) {
            let usd = price.input_per_mtok * (total as f64) / 1_000_000.0;
            println!("\n  at {model}: ~${usd:.5} per turn just to say hello");
            println!("  (~${:.2} across 1,000 turns)", usd * 1000.0);
        }
    }

    println!("\ntop tool schemas by size:");
    for (name, toks, _) in tools.iter().take(10) {
        let bar = "█".repeat(((*toks as f64 / tool_tokens.max(1) as f64) * 40.0) as usize);
        println!("  {toks:>6}  {name:<28} {bar}");
    }

    println!(
        "\nTrim with `--preset review` (or [tools].preset / [tools].disabled_tools),\n\
         or `--json` for the full list.\n\
         MCP servers add their tools to this total the moment they connect."
    );
    Ok(ExitCode::SUCCESS)
}
