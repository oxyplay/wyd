use std::path::Path;

use crate::model::{Category, ProcessInfo};

/// Result of matching a single process against the signature table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Class {
    pub category: Category,
    pub display_name: String,
}

/// Classify one process from name + argv + executable. First match wins;
/// more specific signatures must come first.
pub fn classify(p: &ProcessInfo) -> Option<Class> {
    let name = p.name.to_ascii_lowercase();
    let argv0 = argv0_base(p);
    let cmd = p.command.join(" ").to_ascii_lowercase();
    let exe = p
        .executable
        .as_ref()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let hay = format!("{name} {argv0} {cmd} {exe}");

    if let Some(class) = extra_signature(&name, &argv0, &hay) {
        return Some(class);
    }

    let pkg_manager = name_eq(&name, &argv0, "npm")
        || name_eq(&name, &argv0, "npx")
        || name_eq(&name, &argv0, "pnpm")
        || name_eq(&name, &argv0, "yarn")
        || name_eq(&name, &argv0, "bun")
        || hay.contains("npm-cli.js")
        || hay.contains("npx-cli.js")
        || hay.contains("npm exec");

    // Package-manager wrappers mention the package on argv; the child node
    // is the real server — don't double-count.
    if !pkg_manager {
        for (needle, display) in [
            ("chrome-devtools-mcp", "chrome-devtools-mcp"),
            ("playwright-mcp", "playwright-mcp"),
            ("@playwright/mcp", "playwright-mcp"),
            ("mcp-server-filesystem", "filesystem"),
            ("@modelcontextprotocol/server", "mcp-server"),
            ("context7", "context7"),
            ("queryknight", "queryknight"),
            ("mcp-server", "mcp-server"),
        ] {
            if hay.contains(needle) {
                return Some(Class {
                    category: Category::Mcp,
                    display_name: display.into(),
                });
            }
        }
        if name.contains("-mcp") || argv0.contains("-mcp") || name.contains("mcp-server") {
            return Some(Class {
                category: Category::Mcp,
                display_name: p.label().to_string(),
            });
        }
    }

    // Agents — short names are exact (avoid matching inside other words).
    for (exact, display) in [
        ("omp", "omp"),
        ("opencode", "opencode"),
        ("claude", "claude"),
        ("claude-code", "claude"),
        ("codex", "codex"),
        ("aider", "aider"),
        ("cursor-agent", "cursor"),
        ("gemini", "gemini"),
        ("amp", "amp"),
        ("crush", "crush"),
        ("goose", "goose"),
        ("qwen", "qwen"),
        ("droid", "droid"),
        ("kiro-cli", "kiro"),
        ("agy", "antigravity"),
        ("pi", "pi"),
    ] {
        if name_eq(&name, &argv0, exact) {
            return Some(Class {
                category: Category::Agent,
                display_name: display.into(),
            });
        }
    }

    // Language servers. Skip npm/npx wrappers — the child node is the real LSP.
    if !pkg_manager {
        for (needle, display) in [
            ("rust-analyzer", "rust-analyzer"),
            ("gopls", "gopls"),
            ("typescript-language-server", "typescript"),
            ("copilot-language-server", "copilot"),
            ("pyright-langserver", "pyright"),
            ("pyright", "pyright"),
            ("clangd", "clangd"),
            ("lua-language-server", "lua"),
            ("intelephense", "intelephense"),
            ("vue-language-server", "vue"),
            ("@vue/language-server", "vue"),
            ("svelteserver", "svelte"),
            ("svelte-language-server", "svelte"),
            ("tailwindcss-language-server", "tailwind"),
            ("vscode-eslint-language-server", "eslint"),
            ("eslint_d", "eslint"),
            ("yaml-language-server", "yaml"),
            ("bash-language-server", "bash"),
            ("docker-langserver", "docker"),
            ("jdtls", "jdtls"),
            ("ruby-lsp", "ruby"),
            ("solargraph", "ruby"),
            ("nixd", "nix"),
        ] {
            if name_eq(&name, &argv0, needle) || hay.contains(needle) {
                return Some(Class {
                    category: Category::LanguageServer,
                    display_name: display.into(),
                });
            }
        }
        // Substring-risky short names: exact argv0 only, or an explicit
        // subcommand, so `biome check`/`biome format` are not LSPs.
        if hay.contains("biome lsp-proxy") {
            return Some(Class {
                category: Category::LanguageServer,
                display_name: "biome".into(),
            });
        }
        if name_eq(&name, &argv0, "nil") {
            return Some(Class {
                category: Category::LanguageServer,
                display_name: "nil".into(),
            });
        }
        if hay.contains("language-server") || hay.contains("langserver") {
            return Some(Class {
                category: Category::LanguageServer,
                display_name: p.label().to_string(),
            });
        }
    }

    // Dev servers — before generic node/python.
    // Real Vite is `node …/node_modules/vite/bin/vite.js`; `.bin/vite` is
    // only the symlink form.
    if is_vite(&hay, &name, &argv0) {
        return Some(dev(Category::DevServer, "vite"));
    }
    if hay.contains("next dev") || hay.contains("next-server") || hay.contains("node_modules/next")
    {
        return Some(dev(Category::DevServer, "next"));
    }
    if name_eq(&name, &argv0, "nuxt") || hay.contains("nuxt") && hay.contains("dev") {
        return Some(dev(Category::DevServer, "nuxt"));
    }
    if hay.contains("uvicorn") {
        return Some(dev(Category::DevServer, "uvicorn"));
    }
    if hay.contains("gunicorn") {
        return Some(dev(Category::DevServer, "gunicorn"));
    }
    if hay.contains("manage.py") && hay.contains("runserver") {
        return Some(dev(Category::DevServer, "django"));
    }
    if hay.contains("flask run") {
        return Some(dev(Category::DevServer, "flask"));
    }
    if hay.contains("artisan serve") || hay.contains("php -s") {
        return Some(dev(Category::DevServer, "php"));
    }
    if hay.contains("webpack-dev-server") || hay.contains("webpack serve") {
        return Some(dev(Category::DevServer, "webpack"));
    }
    if name_eq(&name, &argv0, "astro") || hay.contains("astro dev") || hay.contains("@astrojs") {
        return Some(dev(Category::DevServer, "astro"));
    }
    if hay.contains("svelte-kit") || hay.contains("@sveltejs/kit") {
        return Some(dev(Category::DevServer, "svelte-kit"));
    }
    if name_eq(&name, &argv0, "remix") || hay.contains("@remix-run") || hay.contains("react-router")
    {
        return Some(dev(Category::DevServer, "remix"));
    }
    if name_eq(&name, &argv0, "parcel") || hay.contains("parcel-bundler") {
        return Some(dev(Category::DevServer, "parcel"));
    }
    if hay.contains("rsbuild") {
        return Some(dev(Category::DevServer, "rsbuild"));
    }
    if hay.contains("rspack") {
        return Some(dev(Category::DevServer, "rspack"));
    }
    if name_eq(&name, &argv0, "nest")
        || hay.contains("@nestjs")
        || hay.contains("nest start")
        || hay.contains("nest serve")
    {
        return Some(dev(Category::DevServer, "nest"));
    }
    if hay.contains("puma") {
        return Some(dev(Category::DevServer, "puma"));
    }
    if name_eq(&name, &argv0, "rails") || hay.contains("rails s") {
        return Some(dev(Category::DevServer, "rails"));
    }
    if hay.contains("phx.server") || hay.contains("phoenix") {
        return Some(dev(Category::DevServer, "phoenix"));
    }

    // Background workers / watchers — agents start and forget these.
    if !pkg_manager {
        if name_eq(&name, &argv0, "celery") || hay.contains("celery") && hay.contains("worker") {
            return Some(dev(Category::Worker, "celery"));
        }
        if name_eq(&name, &argv0, "sidekiq") || hay.contains("sidekiq") {
            return Some(dev(Category::Worker, "sidekiq"));
        }
        if name_eq(&name, &argv0, "horizon") || hay.contains("artisan horizon") {
            return Some(dev(Category::Worker, "horizon"));
        }
        if hay.contains("queue:work") || hay.contains("artisan queue") {
            return Some(dev(Category::Worker, "laravel queue"));
        }
        if name_eq(&name, &argv0, "nodemon") || hay.contains("nodemon") {
            return Some(dev(Category::Worker, "nodemon"));
        }
        if name_eq(&name, &argv0, "cargo-watch")
            || hay.contains("cargo watch")
            || hay.contains("cargo-watch")
        {
            return Some(dev(Category::Worker, "cargo-watch"));
        }
        if name_eq(&name, &argv0, "watchexec") {
            return Some(dev(Category::Worker, "watchexec"));
        }
        if name_eq(&name, &argv0, "air") {
            return Some(dev(Category::Worker, "air"));
        }
        if hay.contains("tsc --watch")
            || hay.contains("tailwindcss --watch")
            || hay.contains("tailwind --watch")
        {
            return Some(dev(Category::Worker, "watcher"));
        }
    }

    // Databases.
    for (needle, display) in [
        ("postgres", "postgres"),
        ("postgresql", "postgres"),
        ("postmaster", "postgres"),
        ("mysqld", "mysql"),
        ("mariadbd", "mariadb"),
        ("redis-server", "redis"),
        ("mongod", "mongodb"),
        ("elasticsearch", "elasticsearch"),
        ("opensearch", "opensearch"),
        ("clickhouse-server", "clickhouse"),
        ("cockroach", "cockroachdb"),
        ("cassandra", "cassandra"),
        ("memcached", "memcached"),
        ("neo4j", "neo4j"),
        ("qdrant", "qdrant"),
        ("weaviate", "weaviate"),
        ("milvus", "milvus"),
        ("meilisearch", "meilisearch"),
        ("typesense-server", "typesense"),
        ("influxd", "influxdb"),
        ("sqlservr", "sql server"),
    ] {
        if name_eq(&name, &argv0, needle) || hay.contains(needle) {
            return Some(Class {
                category: Category::Database,
                display_name: display.into(),
            });
        }
    }

    if name_eq(&name, &argv0, "php-fpm") || hay.contains("php-fpm") {
        return Some(dev(Category::DevService, "php-fpm"));
    }

    // Browsers (dev-vs-desktop decided later from ancestry).
    if is_chromium_family(&name, &argv0, &hay) {
        return Some(dev(Category::Browser, "Chromium"));
    }
    if name.contains("firefox") || argv0.contains("firefox") {
        return Some(dev(Category::Browser, "Firefox"));
    }

    // Generic JS/Python/PHP runtimes — last, so wrappers can be skipped.
    for (exact, display) in [
        ("node", "node"),
        ("nodejs", "node"),
        ("npm", "npm"),
        ("npx", "npx"),
        ("pnpm", "pnpm"),
        ("yarn", "yarn"),
        ("bun", "bun"),
        ("deno", "deno"),
        ("python", "python"),
        ("python3", "python"),
        ("php", "php"),
    ] {
        if name_eq(&name, &argv0, exact) {
            return Some(dev(Category::UnknownDev, display));
        }
    }

    None
}

fn extra_signature(name: &str, argv0: &str, hay: &str) -> Option<Class> {
    for sig in &crate::config::Config::global().signature {
        let Some(category) = sig.category() else {
            continue;
        };
        let hit = sig
            .names
            .iter()
            .any(|n| name_eq(name, argv0, &n.to_ascii_lowercase()))
            || sig
                .contains
                .iter()
                .any(|n| hay.contains(&n.to_ascii_lowercase()));
        if hit {
            let display = if sig.display.is_empty() {
                name.to_string()
            } else {
                sig.display.clone()
            };
            return Some(Class {
                category,
                display_name: display,
            });
        }
    }
    None
}

fn dev(category: Category, display_name: &str) -> Class {
    Class {
        category,
        display_name: display_name.into(),
    }
}

fn name_eq(name: &str, argv0: &str, exact: &str) -> bool {
    name == exact || argv0 == exact
}

fn argv0_base(p: &ProcessInfo) -> String {
    p.command
        .first()
        .and_then(|s| Path::new(s).file_name())
        .and_then(|s| s.to_str())
        .unwrap_or(&p.name)
        .to_ascii_lowercase()
}

pub(crate) fn is_vite(hay: &str, name: &str, argv0: &str) -> bool {
    name_eq(name, argv0, "vite")
        || hay.contains("node_modules/.bin/vite")
        || hay.contains("node_modules/vite")
        || hay.contains("/vite/bin/vite")
        || hay.contains("vite/dist/node/cli")
        || hay
            .split(['/', '\\', ' ', ':'])
            .any(|p| p == "vite" || p == "vite.js")
}

fn is_chromium_family(name: &str, argv0: &str, hay: &str) -> bool {
    const HINTS: &[&str] = &[
        "chromium",
        "chrome-headless",
        "headless_shell",
        "google chrome",
        "chrome helper",
        "chromedriver",
    ];
    if HINTS
        .iter()
        .any(|h| name.contains(h) || argv0.contains(h) || hay.contains(h))
    {
        return true;
    }
    // Bare "chrome" / "Chrome" process name, not "chrome-devtools-mcp" (already MCP).
    (name == "chrome" || argv0 == "chrome") && !hay.contains("chrome-devtools-mcp")
}

/// True when this browser looks automation/dev-related from its own command.
pub fn browser_cmd_looks_dev(p: &ProcessInfo) -> bool {
    let cmd = p.command.join(" ").to_ascii_lowercase();
    let exe = p
        .executable
        .as_ref()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let hay = format!("{cmd} {exe}");
    hay.contains("playwright")
        || hay.contains("puppeteer")
        || hay.contains("chrome-devtools")
        || hay.contains("remote-debugging")
        || hay.contains("headless")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn proc(name: &str, cmd: &[&str]) -> ProcessInfo {
        ProcessInfo {
            pid: 1,
            parent_pid: None,
            name: name.into(),
            command: cmd.iter().map(|s| (*s).to_string()).collect(),
            executable: cmd.first().map(PathBuf::from),
            cwd: None,
            cpu_percent: 0.0,
            memory_bytes: 0,
            start_time: 0,
            tty: None,
        }
    }

    fn cat(p: ProcessInfo) -> Option<(Category, String)> {
        classify(&p).map(|c| (c.category, c.display_name))
    }

    #[test]
    fn mcp_before_generic_node() {
        let p = proc(
            "node",
            &["node", "/Users/x/.npm/_npx/chrome-devtools-mcp/index.js"],
        );
        assert_eq!(cat(p), Some((Category::Mcp, "chrome-devtools-mcp".into())));
    }

    #[test]
    fn omp_is_exact_not_substring() {
        assert_eq!(
            cat(proc("omp", &["/opt/omp"])),
            Some((Category::Agent, "omp".into()))
        );
        assert_eq!(cat(proc("prompt", &["prompt"])), None);
    }

    #[test]
    fn vite_bin_is_dev_server() {
        let p = proc("node", &["node", "node_modules/.bin/vite"]);
        assert_eq!(cat(p), Some((Category::DevServer, "vite".into())));
        let resolved = proc(
            "node",
            &[
                "node",
                "/Users/x/app/node_modules/vite/bin/vite.js",
                "--port",
                "3000",
            ],
        );
        assert_eq!(cat(resolved), Some((Category::DevServer, "vite".into())));
    }

    #[test]
    fn rust_analyzer_and_postgres() {
        assert_eq!(
            cat(proc("rust-analyzer", &["rust-analyzer"])),
            Some((Category::LanguageServer, "rust-analyzer".into()))
        );
        assert_eq!(
            cat(proc(
                "postgres",
                &["postgres", "-D", "/opt/homebrew/var/postgresql"]
            )),
            Some((Category::Database, "postgres".into()))
        );
    }

    #[test]
    fn copilot_lsp_skips_npm_wrapper() {
        assert_eq!(
            cat(proc(
                "node",
                &[
                    "node",
                    "/Users/x/.npm/_npx/x/node_modules/.bin/copilot-language-server",
                    "--stdio",
                ],
            )),
            Some((Category::LanguageServer, "copilot".into()))
        );
        assert_ne!(
            cat(proc(
                "npm",
                &[
                    "npm",
                    "exec",
                    "@github/copilot-language-server@^1.408.0",
                    "--stdio",
                ],
            ))
            .map(|(c, _)| c),
            Some(Category::LanguageServer)
        );
        assert_ne!(
            cat(proc(
                "node",
                &[
                    "node",
                    "/opt/homebrew/lib/node_modules/npm/bin/npm-cli.js",
                    "exec",
                    "@github/copilot-language-server@^1.408.0",
                    "--stdio",
                ],
            ))
            .map(|(c, _)| c),
            Some(Category::LanguageServer)
        );
        assert_ne!(
            cat(proc(
                "node",
                &["npm exec @github/copilot-language-server@^1.408.0 --stdio"],
            ))
            .map(|(c, _)| c),
            Some(Category::LanguageServer)
        );
        assert_eq!(
            cat(proc(
                "typescript-language-server",
                &["typescript-language-server", "--stdio"],
            )),
            Some((Category::LanguageServer, "typescript".into()))
        );
    }

    #[test]
    fn chromium_family() {
        assert_eq!(
            cat(proc("Chromium", &["/app/Chromium"])),
            Some((Category::Browser, "Chromium".into()))
        );
        assert_eq!(
            cat(proc("chrome", &["chrome", "--headless"])),
            Some((Category::Browser, "Chromium".into()))
        );
    }

    #[test]
    fn new_agents() {
        for (bin, display) in [
            ("amp", "amp"),
            ("crush", "crush"),
            ("goose", "goose"),
            ("qwen", "qwen"),
            ("droid", "droid"),
            ("kiro-cli", "kiro"),
            ("agy", "antigravity"),
            ("pi", "pi"),
        ] {
            assert_eq!(
                cat(proc(bin, &[bin])),
                Some((Category::Agent, display.into())),
                "{bin} should be an agent"
            );
        }
        // Not a substring match: `gemini` must not match `gemini-code`.
        assert_ne!(
            cat(proc("qwen-code", &["qwen-code"])).map(|(c, _)| c),
            Some(Category::Agent)
        );
    }

    #[test]
    fn vector_and_search_databases() {
        for bin in [
            "qdrant",
            "weaviate",
            "milvus",
            "elasticsearch",
            "opensearch",
            "clickhouse-server",
            "cockroach",
            "cassandra",
            "memcached",
            "neo4j",
            "meilisearch",
            "typesense-server",
            "influxd",
            "sqlservr",
        ] {
            assert_eq!(
                cat(proc(bin, &[bin])).map(|(c, _)| c),
                Some(Category::Database),
                "{bin} should be a database"
            );
        }
    }

    #[test]
    fn new_language_servers() {
        for bin in [
            "vue-language-server",
            "svelteserver",
            "tailwindcss-language-server",
            "vscode-eslint-language-server",
            "yaml-language-server",
            "bash-language-server",
            "docker-langserver",
            "jdtls",
            "ruby-lsp",
            "solargraph",
            "nixd",
        ] {
            assert_eq!(
                cat(proc(bin, &[bin])).map(|(c, _)| c),
                Some(Category::LanguageServer),
                "{bin} should be a language server"
            );
        }
        // biome is only an LSP for its lsp-proxy subcommand, not `check`.
        assert_eq!(
            cat(proc("biome", &["biome", "lsp-proxy"])).map(|(c, _)| c),
            Some(Category::LanguageServer)
        );
        assert_ne!(
            cat(proc("biome", &["biome", "check"])).map(|(c, _)| c),
            Some(Category::LanguageServer)
        );
        // `nil` is exact-only (substring is too risky).
        assert_eq!(
            cat(proc("nil", &["nil"])).map(|(c, _)| c),
            Some(Category::LanguageServer)
        );
        assert_ne!(
            cat(proc("terminal", &["terminal"])).map(|(c, _)| c),
            Some(Category::LanguageServer)
        );
    }

    #[test]
    fn background_workers() {
        for (name, cmd, _display) in [
            ("celery", &["celery", "-A", "proj", "worker"][..], "celery"),
            ("sidekiq", &["ruby", "bin/sidekiq"][..], "sidekiq"),
            (
                "node",
                &["node", "node_modules/nodemon/bin/nodemon.js"][..],
                "nodemon",
            ),
            (
                "cargo-watch",
                &["cargo-watch", "-x", "run"][..],
                "cargo-watch",
            ),
            ("watchexec", &["watchexec", "make"][..], "watchexec"),
            ("air", &["air"][..], "air"),
            (
                "node",
                &["node", "node_modules/typescript/bin/tsc", "--watch"][..],
                "watcher",
            ),
        ] {
            assert_eq!(
                cat(proc(name, cmd)).map(|(c, _)| c),
                Some(Category::Worker),
                "{cmd:?} should be a worker"
            );
        }
    }

    #[test]
    fn new_dev_servers() {
        for (name, cmd, _display) in [
            (
                "node",
                &["node", "node_modules/.bin/astro", "dev"][..],
                "astro",
            ),
            (
                "node",
                &["node", "node_modules/@sveltejs/kit/bin/kit.js"][..],
                "svelte-kit",
            ),
            (
                "node",
                &["node", "node_modules/@remix-run/dev/dist/cli.js"][..],
                "remix",
            ),
            (
                "node",
                &["node", "node_modules/.bin/nest", "start"][..],
                "nest",
            ),
            ("puma", &["puma"][..], "puma"),
            ("rails", &["rails", "server"][..], "rails"),
            ("beam.smp", &["beam.smp", "phx.server"][..], "phoenix"),
        ] {
            assert_eq!(
                cat(proc(name, cmd)).map(|(c, _)| c),
                Some(Category::DevServer),
                "{cmd:?} should be a dev server"
            );
        }
    }
}
