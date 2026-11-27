use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::client::PicusClient;

const MAX_BUNDLE_BYTES: usize = 15 * 1024 * 1024;

const REACT_TEMPLATE: &[(&str, &str)] = &[
    ("picus.app.toml", include_str!("templates/react/picus.app.toml")),
    ("package.json", include_str!("templates/react/package.json")),
    ("vite.config.ts", include_str!("templates/react/vite.config.ts")),
    ("tsconfig.json", include_str!("templates/react/tsconfig.json")),
    ("index.html", include_str!("templates/react/index.html")),
    ("runtime-config.dev.json", include_str!("templates/react/runtime-config.dev.json")),
    (".gitignore", include_str!("templates/react/gitignore")),
    ("src/main.tsx", include_str!("templates/react/src/main.tsx")),
    ("src/App.tsx", include_str!("templates/react/src/App.tsx")),
    ("src/index.css", include_str!("templates/react/src/index.css")),
    ("src/picusbase.ts", include_str!("templates/react/src/picusbase.ts")),
];

#[derive(Deserialize)]
struct AppManifest {
    app: AppSection,
    build: BuildSection,
}

#[derive(Deserialize)]
struct AppSection {
    code: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    framework: String,
    #[serde(default)]
    project_ref: String,
    #[serde(default)]
    domain: String,
}

#[derive(Deserialize)]
struct BuildSection {
    command: String,
    output: String,
}

fn validate_code(code: &str) -> Result<()> {
    let valid = code.len() <= 63
        && code.starts_with(|c: char| c.is_ascii_lowercase())
        && code.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !valid {
        bail!("App code '{code}' must start with a lowercase letter and contain only a-z, 0-9, '_' (max 63)");
    }
    Ok(())
}

fn non_empty(value: String) -> Option<String> {
    Some(value).filter(|v| !v.trim().is_empty())
}

fn console_host(console_url: &str) -> &str {
    let rest = console_url.split_once("://").map(|(_, rest)| rest).unwrap_or(console_url);
    let authority = rest.split('/').next().unwrap_or(rest);
    authority.split(':').next().unwrap_or(authority)
}

pub fn init_app(
    client: &PicusClient,
    project_ref: Option<&str>,
    code: &str,
    framework: &str,
    name: &str,
    domain: Option<String>,
    dir: Option<PathBuf>,
) -> Result<()> {
    if framework != "react" {
        bail!("Unsupported framework '{framework}' (supported: react)");
    }
    validate_code(code)?;
    if name.contains(['<', '>', '{', '}', '"', '\\']) {
        bail!("App name must not contain < > {{ }} \" or \\");
    }
    let project_ref = project_ref.context(
        "project_ref is required: --project-ref or PICUSBASE_PROJECT_REF",
    )?;
    let kebab = code.replace('_', "-");
    let console_url = client.base_url().trim_end_matches('/');
    let domain = domain.unwrap_or_else(|| {
        if console_host(console_url).ends_with(".localhost") {
            format!("{kebab}.picus.localhost")
        } else {
            String::new()
        }
    });
    let functions_url = client.functions_url().unwrap_or_default();
    let dir = dir.unwrap_or_else(|| PathBuf::from(&kebab));
    if dir.exists() && dir.read_dir()?.next().is_some() {
        bail!("{} already exists and is not empty", dir.display());
    }

    for (relative, content) in REACT_TEMPLATE {
        let target = dir.join(relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let rendered = content
            .replace("{{APP_CODE}}", code)
            .replace("{{APP_NAME}}", name)
            .replace("{{PACKAGE_NAME}}", &kebab)
            .replace("{{PROJECT_REF}}", project_ref)
            .replace("{{DOMAIN}}", &domain)
            .replace("{{CONSOLE_URL}}", console_url)
            .replace("{{FUNCTIONS_URL}}", &functions_url);
        std::fs::write(&target, rendered).with_context(|| format!("Failed to write {}", target.display()))?;
    }

    println!("✅ Created React + Vite app '{code}' in {}", dir.display());
    println!("   cd {}", dir.display());
    println!("   npm install && npm run dev      # local preview");
    println!("   picusbase deploy app            # build + deploy{}", if domain.is_empty() { " (set [app].domain in picus.app.toml first)".to_string() } else { format!(" to http://{domain}") });
    Ok(())
}

fn run_shell(dir: &Path, command: &str) -> Result<()> {
    println!("$ {command}");
    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/C", command]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", command]);
        c
    };
    let status = cmd.current_dir(dir).status().with_context(|| format!("Failed to run `{command}`"))?;
    if !status.success() {
        bail!("`{command}` failed ({status})");
    }
    Ok(())
}

fn pack_dir(dir: &Path) -> Result<Vec<u8>> {
    let encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let mut archive = tar::Builder::new(encoder);
    archive.append_dir_all(".", dir)?;
    Ok(archive.into_inner()?.finish()?)
}

/// Tags and trimmed text between them.
fn html_tokens(html: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let mut rest = html;
    while !rest.is_empty() {
        let end = if rest.starts_with('<') { rest.find('>').map_or(rest.len(), |i| i + 1) } else { rest.find('<').unwrap_or(rest.len()) };
        let token = rest[..end].trim();
        if !token.is_empty() {
            tokens.push(token);
        }
        rest = &rest[end..];
    }
    tokens
}

/// Every tag and text of the built page, in order. The host and proxies add elements (a CDN such as
/// Cloudflare injects its analytics script), so the served page is never byte-equal to the build.
fn serves_build(served: &str, built: &str) -> bool {
    let mut served = html_tokens(served).into_iter();
    html_tokens(built).into_iter().all(|token| served.any(|s| s == token))
}

/// Waits until the domain serves this build's index.html (a redeploy keeps the old pod serving
/// until the rollout finishes).
fn wait_until_serving(url: &str, expected_index: &str) -> bool {
    let http = match reqwest::blocking::Client::builder().timeout(Duration::from_secs(5)).build() {
        Ok(http) => http,
        Err(_) => return false,
    };
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        let served = http
            .get(url)
            .header("accept", "text/html")
            .send()
            .ok()
            .filter(|res| res.status().is_success())
            .and_then(|res| res.text().ok())
            .is_some_and(|html| serves_build(&html, expected_index));
        if served {
            return true;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    false
}

fn require_project_ref(project_ref: Option<&str>) -> Result<&str> {
    project_ref.context("project_ref is required: --project-ref or PICUSBASE_PROJECT_REF")
}

/// Waits until `url` serves this app's container (`/runtime-config.json` names the app).
fn wait_until_app_served(url: &str, app_code: &str) -> bool {
    let http = match reqwest::blocking::Client::builder().timeout(Duration::from_secs(5)).build() {
        Ok(http) => http,
        Err(_) => return false,
    };
    let config_url = format!("{}/runtime-config.json", url.trim_end_matches('/'));
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline {
        let served = http
            .get(&config_url)
            .send()
            .ok()
            .filter(|res| res.status().is_success())
            .and_then(|res| res.json::<serde_json::Value>().ok())
            .is_some_and(|config| config.get("pinned_app_code").and_then(|v| v.as_str()) == Some(app_code));
        if served {
            return true;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    false
}

pub fn list_domains(client: &PicusClient, project_ref: Option<&str>) -> Result<()> {
    let project_ref = require_project_ref(project_ref)?;
    let apps = client.list_apps(project_ref)?;
    if apps.is_empty() {
        println!("No Apps in {project_ref}.");
        return Ok(());
    }
    let text = |app: &serde_json::Value, key: &str| app.get(key).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let rows: Vec<[String; 4]> = apps
        .iter()
        .map(|app| {
            let domain = text(app, "domain");
            [
                text(app, "app_code"),
                text(app, "app_name"),
                text(app, "ui_framework"),
                if domain.is_empty() { "-".to_string() } else { format!("http://{domain}/") },
            ]
        })
        .collect();
    let header = ["APP_CODE", "NAME", "FRAMEWORK", "URL"];
    let widths: Vec<usize> = (0..4)
        .map(|i| rows.iter().map(|r| r[i].chars().count()).chain([header[i].len()]).max().unwrap_or(0))
        .collect();
    let line = |cells: [&str; 4]| {
        cells.iter().zip(&widths).map(|(c, w)| format!("{c:<w$}")).collect::<Vec<_>>().join("  ").trim_end().to_string()
    };
    println!("{}", line(header));
    for row in &rows {
        println!("{}", line([&row[0], &row[1], &row[2], &row[3]]));
    }
    Ok(())
}

pub fn set_domain(client: &PicusClient, project_ref: Option<&str>, app_code: &str, domain: &str) -> Result<()> {
    let project_ref = require_project_ref(project_ref)?;
    let domain = domain.trim().trim_end_matches('.').to_lowercase();
    let app = client
        .get_app(project_ref, app_code)?
        .with_context(|| format!("App '{app_code}' is not registered in {project_ref} (see `picusbase domain list`)"))?;
    let previous = app.get("domain").and_then(|v| v.as_str()).map(str::to_string);
    println!("🚀 {} '{app_code}' → {domain}...", if previous.is_some() { "Moving" } else { "Deploying" });
    let result = client.deploy_app(project_ref, app_code, &domain, None)?;
    let url = result.get("url").and_then(|v| v.as_str()).map(str::to_string).unwrap_or_else(|| format!("http://{domain}/"));
    if wait_until_app_served(&url, app_code) {
        println!("✅ {url}");
    } else {
        println!("⚠️  Deployed, but {url} is not serving yet — DNS for {domain} must point at the cluster ingress");
    }
    if let Some(previous) = previous.filter(|previous| previous != &domain) {
        println!("   {previous} no longer serves this app");
    }
    Ok(())
}

pub fn remove_domain(client: &PicusClient, project_ref: Option<&str>, app_code: &str) -> Result<()> {
    let project_ref = require_project_ref(project_ref)?;
    client.undeploy_app(project_ref, app_code)?;
    println!("✅ '{app_code}' undeployed — `picusbase domain set {app_code} <domain>` brings it back");
    Ok(())
}

pub fn deploy_app(
    client: &PicusClient,
    explicit_project_ref: Option<&str>,
    fallback_project_ref: Option<&str>,
    path: &Path,
    domain_override: Option<String>,
    skip_build: bool,
) -> Result<()> {
    let manifest_path = path.join("picus.app.toml");
    let manifest: AppManifest = toml::from_str(
        &std::fs::read_to_string(&manifest_path)
            .with_context(|| format!("{} not found — run inside an app created by `picusbase init app`", manifest_path.display()))?,
    )
    .with_context(|| format!("Invalid {}", manifest_path.display()))?;
    let code = manifest.app.code;
    validate_code(&code)?;
    let project_ref = explicit_project_ref
        .map(str::to_string)
        .or_else(|| non_empty(manifest.app.project_ref))
        .or_else(|| fallback_project_ref.map(str::to_string))
        .context("project_ref is required: set [app].project_ref in picus.app.toml or --project-ref")?;
    let existing = client.get_app(&project_ref, &code)?;
    let live_domain = existing
        .as_ref()
        .and_then(|app| app.get("domain").and_then(|v| v.as_str()))
        .map(str::to_string);
    // A domain set later (`picusbase domain set`, Console) wins over the one in picus.app.toml.
    let domain = domain_override
        .or(live_domain)
        .or_else(|| non_empty(manifest.app.domain))
        .context("domain is required: set [app].domain in picus.app.toml or pass --domain")?;

    if !skip_build {
        // npm workspaces hoist node_modules to the workspace root.
        let absolute = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        if !absolute.ancestors().any(|dir| dir.join("node_modules").is_dir()) {
            run_shell(path, "npm install")?;
        }
        run_shell(path, &manifest.build.command)?;
    }
    let output = path.join(&manifest.build.output);
    let expected_index = std::fs::read_to_string(output.join("index.html"))
        .with_context(|| format!("{} not found — the build did not produce index.html", output.join("index.html").display()))?;

    let bundle = pack_dir(&output)?;
    if bundle.len() > MAX_BUNDLE_BYTES {
        bail!("Bundle is {} bytes, over the {MAX_BUNDLE_BYTES}-byte limit", bundle.len());
    }
    let name = non_empty(manifest.app.name).unwrap_or_else(|| code.clone());
    let framework = non_empty(manifest.app.framework).unwrap_or_else(|| "react".to_string());
    if let Some(app) = &existing {
        let existing_framework = app.get("ui_framework").and_then(|v| v.as_str()).unwrap_or("dioxus");
        // "dioxus" = legacy app of the removed runtime; it has nothing to serve, so it can be replaced.
        if existing_framework != framework && existing_framework != "dioxus" {
            bail!("App code '{code}' is already used by a {existing_framework} app in {project_ref} — pick another [app].code");
        }
    }
    client.define_app(&project_ref, &code, &name, &framework)?;
    println!("📤 Uploading bundle ({} KB)...", bundle.len() / 1024);
    let checksum = client.upload_app_bundle(&project_ref, &code, bundle)?;
    println!("🚀 Deploying '{code}' to {domain} (project {project_ref})...");
    let result = client.deploy_app(&project_ref, &code, &domain, Some(&checksum))?;
    let url = result
        .get("url")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| format!("http://{domain}/"));

    if wait_until_serving(&url, &expected_index) {
        println!("✅ {url}");
    } else {
        println!("⚠️  Deployed, but {url} is not serving yet — check `kubectl get pods -n picusbase | grep app-`");
    }
    Ok(())
}
