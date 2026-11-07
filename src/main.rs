mod api;
mod app_commands;
mod app_config;
mod bundler;
mod client;
mod commands;
mod config;
mod language;
mod gen_types;
mod manifest;
mod schema_deploy;
mod web_app;

use anyhow::{Context, Result};
use clap::Parser;
use client::{PicusClient, PublishExtras};
use commands::{
    ApiCommands, AppCommands, AppFunctionsCommands, BundleCommands, Cli,
    ClusterCommands, Commands, ConfigCommands, DataCommands, DeployCommands, DevCommands, DomainCommands, FunctionCommands,
    GenCommands,
    InitCommands, JobCommands, PlatformCommands, RouteCommands, ScheduleCommands,
};
use config::{CliDefaults, CliProfile, PicusConfig};
use language::{adapter_for, detect_language, Language};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tabled::Table;

fn normalize_optional_string(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn resolve_explicit_cli_profile(cli_profile: Option<String>) -> Option<String> {
    normalize_optional_string(cli_profile)
        .or_else(|| normalize_optional_string(std::env::var("PICUS_PROFILE").ok()))
}

fn resolve_cli_profile(cli_profile: Option<String>, defaults: &CliDefaults) -> Option<String> {
    resolve_explicit_cli_profile(cli_profile)
        .or_else(|| normalize_optional_string(defaults.default_profile.clone()))
}

fn resolve_selected_profile<'a>(
    profile_name: Option<&str>,
    defaults: &'a CliDefaults,
) -> Result<Option<&'a CliProfile>> {
    match profile_name {
        Some(name) => defaults.profiles.get(name).map(Some).ok_or_else(|| {
            let available = if defaults.profiles.is_empty() {
                "<none>".to_string()
            } else {
                defaults
                    .profiles
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            anyhow::anyhow!(
                "Profile '{}' was not found in the CLI config. Available profiles: {}",
                name,
                available
            )
        }),
        None => Ok(None),
    }
}

fn resolve_cli_url(
    cli_url: Option<String>,
    defaults: &CliDefaults,
    selected_profile: Option<&CliProfile>,
) -> String {
    normalize_optional_string(cli_url)
        .or_else(|| normalize_optional_string(std::env::var("PICUS_URL").ok()))
        .or_else(|| {
            selected_profile.and_then(|profile| normalize_optional_string(profile.url.clone()))
        })
        .or_else(|| normalize_optional_string(defaults.url.clone()))
        .unwrap_or_else(|| CliDefaults::default_api_url().to_string())
}

fn resolve_cli_project_ref(
    cli_project_ref: Option<String>,
    defaults: &CliDefaults,
    selected_profile: Option<&CliProfile>,
) -> Option<String> {
    normalize_optional_string(cli_project_ref)
        .or_else(|| normalize_optional_string(std::env::var("PICUS_PROJECT_REF").ok()))
        .or_else(|| {
            selected_profile
                .and_then(|profile| normalize_optional_string(profile.project_ref.clone()))
        })
        .or_else(|| normalize_optional_string(defaults.project_ref.clone()))
}

fn resolve_cli_executor_url(
    cli_executor_url: Option<String>,
    defaults: &CliDefaults,
    selected_profile: Option<&CliProfile>,
) -> Option<String> {
    normalize_optional_string(cli_executor_url)
        .or_else(|| normalize_optional_string(std::env::var("PICUS_EXECUTOR_URL").ok()))
        .or_else(|| {
            selected_profile
                .and_then(|profile| normalize_optional_string(profile.executor_url.clone()))
        })
        .or_else(|| normalize_optional_string(defaults.executor_url.clone()))
}

fn resolve_cli_personal_token(
    defaults: &CliDefaults,
    selected_profile: Option<&CliProfile>,
) -> Option<String> {
    normalize_optional_string(std::env::var("PICUS_PERSONAL_TOKEN").ok())
        .or_else(|| {
            selected_profile
                .and_then(|profile| normalize_optional_string(profile.personal_token.clone()))
        })
        .or_else(|| normalize_optional_string(defaults.personal_token.clone()))
}

/// Accepts either the function key (picus.toml `id`) or the registry UUID.
fn resolve_function_uuid(client: &PicusClient, function: &str) -> Result<String> {
    client
        .list_compute_functions()?
        .into_iter()
        .find(|f| f.function_key == function || f.id == function)
        .map(|f| f.id)
        .with_context(|| format!("Function '{}' not found in this project", function))
}

fn token_state(token: &Option<String>) -> &'static str {
    if token.is_some() {
        "<set>"
    } else {
        "<unset>"
    }
}

fn resolve_cli_auth_token(cli_auth_token: Option<String>) -> Option<String> {
    normalize_optional_string(cli_auth_token)
        .or_else(|| normalize_optional_string(std::env::var("PICUS_AUTH_TOKEN").ok()))
}

fn find_functions_root(path: &Path) -> Option<&Path> {
    path.ancestors()
        .find(|ancestor| ancestor.file_name().and_then(|name| name.to_str()) == Some("functions"))
}

fn resolve_absolute_path(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        return path;
    }

    match std::env::current_dir() {
        Ok(cwd) => cwd.join(path),
        Err(_) => path,
    }
}

fn shared_functions_target_dir(path: &Path) -> PathBuf {
    let resolved_path = resolve_absolute_path(path.to_path_buf());

    let shared_target = find_functions_root(&resolved_path)
        .map(|root| root.join("target"))
        .unwrap_or_else(|| resolved_path.join("target"));

    shared_target
        .canonicalize()
        .unwrap_or_else(|_| resolve_absolute_path(shared_target))
}

fn collect_function_dirs(path: &Path, function_dirs: &mut Vec<PathBuf>) -> Result<()> {
    if path.join("picus.toml").exists() {
        function_dirs.push(path.to_path_buf());
        return Ok(());
    }

    let entries = std::fs::read_dir(path).context("Failed to read directory")?;
    for entry in entries {
        let entry = entry?;
        let sub_path = entry.path();

        if !sub_path.is_dir() {
            continue;
        }

        if sub_path.file_name().and_then(|name| name.to_str()) == Some("target") {
            continue;
        }

        collect_function_dirs(&sub_path, function_dirs)?;
    }

    Ok(())
}

fn find_repo_root(path: &Path) -> Option<PathBuf> {
    path.ancestors().find_map(|ancestor| {
        let platform_dir = if ancestor.join("picusbase").is_dir() {
            Some(ancestor.join("picusbase"))
        } else if ancestor.join("picus-platform").is_dir() {
            Some(ancestor.join("picus-platform"))
        } else {
            None
        };
        // A Cargo.toml used to sit at the monorepo root (a picus-erp workspace
        // umbrella, unrelated to picusbase's own build) and this check
        // originally used its presence as a repo-root signal — removed in a
        // 2026-09-17 refactor with no replacement, silently breaking every
        // CLI command that needs the repo root on any checkout without that
        // now-gone file. The actual manifest this CLI cares about has always
        // been the platform workspace's own `Cargo.toml`, so check there
        // instead of at the ancestor itself.
        let has_cargo_toml = ancestor.join("Cargo.toml").exists()
            || platform_dir.as_ref().is_some_and(|dir| dir.join("Cargo.toml").exists());
        if has_cargo_toml && ancestor.join("picus-erp").is_dir() && platform_dir.is_some() {
            Some(ancestor.to_path_buf())
        } else {
            None
        }
    })
}

fn platform_repo_dir(repo_root: &Path) -> PathBuf {
    let target = repo_root.join("picusbase");
    if target.is_dir() {
        target
    } else {
        repo_root.join("picus-platform")
    }
}

fn find_managed_functions_root(path: &Path) -> Option<PathBuf> {
    let repo_root = find_repo_root(path)?;
    find_functions_root(path)
        .filter(|root| root.starts_with(&repo_root))
        .map(|root| root.to_path_buf())
}

fn find_platform_dir(cwd: &Path) -> Result<PathBuf> {
    let repo_root = find_repo_root(cwd).ok_or_else(|| {
        anyhow::anyhow!(
            "Could not locate repo root from {}. \
             Run this command from inside the Picus monorepo (must contain picus-erp/ and picusbase/).",
            cwd.display()
        )
    })?;
    Ok(platform_repo_dir(&repo_root))
}

/// Step 3 of the deploy flow: calls kube-manager's `POST /cluster/install`
/// or `/cluster/upgrade` (see `services/kube-manager/src/cluster.rs`), which
/// itself runs the full `helm upgrade --install` (plus data-plane
/// provisioning, unless `skip_data_plane`) on the release Bootstrap created.
fn run_cluster_apply(
    action: &str,
    kube_manager_url: &Option<String>,
    token: &Option<String>,
    values_file: &str,
    namespace: &str,
    release: &str,
    skip_data_plane: bool,
) -> Result<()> {
    let base = kube_manager_url
        .clone()
        .unwrap_or_else(|| "http://localhost:4090".to_string());
    let auth_token = token
        .clone()
        .or_else(|| std::env::var("KUBE_MANAGER_BOOTSTRAP_TOKEN").ok())
        .ok_or_else(|| {
            anyhow::anyhow!("Missing bootstrap token — pass --token or set KUBE_MANAGER_BOOTSTRAP_TOKEN")
        })?;

    let body = serde_json::json!({
        "values_file": values_file,
        "namespace": namespace,
        "release": release,
        "skip_data_plane": skip_data_plane,
    });

    println!("Calling kube-manager {base}/cluster/{action} (this can take several minutes) ...");
    let resp = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(20 * 60))
        .build()
        .context("Failed to build HTTP client")?
        .post(format!("{base}/cluster/{action}"))
        .bearer_auth(&auth_token)
        .json(&body)
        .send()
        .context("Failed to call kube-manager")?;

    let status = resp.status();
    let text = resp.text().unwrap_or_default();
    if !status.is_success() {
        anyhow::bail!("kube-manager /cluster/{action} failed ({status}): {text}");
    }

    println!("{text}");
    Ok(())
}

fn infer_domain_from_path(path: &Path, functions_root: &Path) -> Option<String> {
    let relative = path.strip_prefix(functions_root).ok()?;
    relative
        .components()
        .next()
        .and_then(|component| component.as_os_str().to_str())
        .map(|value| value.to_string())
}

fn scaffold_parent_dir(current_dir: &Path, functions_root: Option<&Path>, domain: &str) -> PathBuf {
    match functions_root {
        Some(root) if current_dir == root => root.join(domain),
        _ => current_dir.to_path_buf(),
    }
}

fn diff_paths(path: &Path, base: &Path) -> Option<PathBuf> {
    use std::path::Component;

    let path_components: Vec<_> = path.components().collect();
    let base_components: Vec<_> = base.components().collect();

    let mut common_len = 0usize;
    while common_len < path_components.len()
        && common_len < base_components.len()
        && path_components[common_len] == base_components[common_len]
    {
        common_len += 1;
    }

    if common_len == 0 {
        return None;
    }

    let mut relative = PathBuf::new();

    for component in &base_components[common_len..] {
        match component {
            Component::Normal(_) | Component::CurDir | Component::ParentDir => relative.push(".."),
            Component::RootDir => {}
            Component::Prefix(_) => return None,
        }
    }

    for component in &path_components[common_len..] {
        relative.push(component.as_os_str());
    }

    if relative.as_os_str().is_empty() {
        relative.push(".");
    }

    Some(relative)
}

fn normalize_toml_path(path: PathBuf) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn relative_dependency_path(from_dir: &Path, target: &Path) -> Result<String> {
    let from_dir = resolve_absolute_path(from_dir.to_path_buf());
    let target = resolve_absolute_path(target.to_path_buf());

    let relative = diff_paths(&target, &from_dir).with_context(|| {
        format!(
            "Failed to compute relative path from {} to {}",
            from_dir.display(),
            target.display()
        )
    })?;

    Ok(normalize_toml_path(relative))
}

fn rust_scaffold_dependency_paths(
    repo_root: &Path,
    function_dir: &Path,
) -> Result<(String, String)> {
    let function_kit_path = relative_dependency_path(
        function_dir,
        &repo_root
            .join("picus-erp")
            .join("packages")
            .join("function-kit"),
    )?;
    let wasm_sdk_path = relative_dependency_path(
        function_dir,
        &platform_repo_dir(repo_root)
            .join("packages")
            .join("wasm-sdk"),
    )?;

    Ok((function_kit_path, wasm_sdk_path))
}

fn read_project_ref_from_picus_toml(path: &Path) -> Option<String> {
    let content = std::fs::read_to_string(path).ok()?;
    let value: toml::Value = toml::from_str(&content).ok()?;
    value
        .get("function")
        .and_then(|function| function.get("project_ref"))
        .and_then(|project_ref| project_ref.as_str())
        .map(|project_ref| project_ref.to_string())
}

fn infer_project_ref(functions_root: &Path, domain: &str) -> Option<String> {
    let domain_dir = functions_root.join(domain);
    if domain_dir.is_dir() {
        let entries = std::fs::read_dir(&domain_dir).ok()?;
        for entry in entries.flatten() {
            let picus_toml = entry.path().join("picus.toml");
            if picus_toml.exists() {
                if let Some(project_ref) = read_project_ref_from_picus_toml(&picus_toml) {
                    return Some(project_ref);
                }
            }
        }
    }

    let entries = std::fs::read_dir(functions_root).ok()?;
    for entry in entries.flatten() {
        let domain_path = entry.path();
        if !domain_path.is_dir() {
            continue;
        }
        let domain_entries = std::fs::read_dir(&domain_path).ok()?;
        for function_entry in domain_entries.flatten() {
            let picus_toml = function_entry.path().join("picus.toml");
            if picus_toml.exists() {
                if let Some(project_ref) = read_project_ref_from_picus_toml(&picus_toml) {
                    return Some(project_ref);
                }
            }
        }
    }

    None
}

fn title_case(value: &str) -> String {
    value
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|segment| !segment.is_empty())
        .map(|segment| {
            let mut chars = segment.chars();
            match chars.next() {
                Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn pascal_case(value: &str) -> String {
    value
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|segment| !segment.is_empty())
        .map(|segment| {
            let mut chars = segment.chars();
            match chars.next() {
                Some(first) => format!("{}{}", first.to_uppercase(), chars.as_str()),
                None => String::new(),
            }
        })
        .collect::<String>()
}

fn prompt_with_default(label: &str, default: &str) -> Result<String> {
    print!("{} (default: {}): ", label, default);
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    let trimmed = input.trim();

    if trimmed.is_empty() {
        Ok(default.to_string())
    } else {
        Ok(trimmed.to_string())
    }
}

fn optional_function_dependencies(
    repo_root: Option<&Path>,
    function_dir: &Path,
    domain: &str,
) -> Result<String> {
    let mut lines = Vec::new();

    if let Some(root) = repo_root {
        let packages_dir = root.join("picus-erp").join("packages");
        if packages_dir.join("dashboard-api-contracts").is_dir() {
            lines.push(format!(
                "dashboard-api-contracts = {{ path = \"{}\" }}",
                relative_dependency_path(
                    function_dir,
                    &packages_dir.join("dashboard-api-contracts")
                )?
            ));
        }

        let support_package = format!("{}-function-support", domain);
        if packages_dir.join(&support_package).is_dir() {
            lines.push(format!(
                "{} = {{ path = \"{}\" }}",
                support_package,
                relative_dependency_path(function_dir, &packages_dir.join(&support_package))?
            ));
        }
    }

    if lines.is_empty() {
        Ok(String::new())
    } else {
        Ok(lines.join("\n"))
    }
}

fn insert_toml_array_entry(content: &mut String, array_name: &str, entry: &str) -> Result<bool> {
    let quoted_entry = format!("\"{}\"", entry);
    if content.contains(&quoted_entry) {
        return Ok(false);
    }

    let array_index = content
        .find(&format!("{} = [", array_name))
        .with_context(|| format!("Could not find {} array in Cargo.toml", array_name))?;
    let after_array = &content[array_index..];
    let closing_index = after_array
        .find("\n]")
        .with_context(|| format!("Could not find the end of the {} array", array_name))?;
    let insert_at = array_index + closing_index + 1;

    content.insert_str(insert_at, &format!("    {},\n", quoted_entry));
    Ok(true)
}

fn add_functions_workspace_domain(functions_root: &Path, domain: &str) -> Result<bool> {
    let cargo_toml_path = functions_root.join("Cargo.toml");
    let mut content = std::fs::read_to_string(&cargo_toml_path)
        .with_context(|| format!("Failed to read {}", cargo_toml_path.display()))?;

    let mut modified = false;
    modified |= insert_toml_array_entry(&mut content, "members", &format!("{}/*", domain))?;
    modified |= insert_toml_array_entry(&mut content, "exclude", &format!("{}/target", domain))?;

    if modified {
        std::fs::write(&cargo_toml_path, content)
            .with_context(|| format!("Failed to update {}", cargo_toml_path.display()))?;
    }

    Ok(modified)
}

/// Insert a single explicit path into the functions workspace
/// `[workspace].exclude` array. Used by non-Cargo function lanes (Python,
/// JavaScript bundle) so cargo metadata does not error on dirs without
/// Cargo.toml that match the broader `members = ["<domain>/*"]` glob.
fn add_functions_workspace_exclude_entry(functions_root: &Path, entry: &str) -> Result<bool> {
    let cargo_toml_path = functions_root.join("Cargo.toml");
    let mut content = std::fs::read_to_string(&cargo_toml_path)
        .with_context(|| format!("Failed to read {}", cargo_toml_path.display()))?;
    let modified = insert_toml_array_entry(&mut content, "exclude", entry)?;
    if modified {
        std::fs::write(&cargo_toml_path, content)
            .with_context(|| format!("Failed to update {}", cargo_toml_path.display()))?;
    }
    Ok(modified)
}

#[derive(Default)]
struct FunctionsWorkspaceBootstrap {
    created_manifest: bool,
    created_build_config: bool,
    excluded_legacy_crates: Vec<String>,
}

fn flat_rust_function_dirs(functions_root: &Path) -> Result<Vec<String>> {
    let mut entries = Vec::new();

    for entry in std::fs::read_dir(functions_root)
        .with_context(|| format!("Failed to read {}", functions_root.display()))?
    {
        let entry = entry?;
        let path = entry.path();

        if !path.is_dir() {
            continue;
        }

        let name = match path.file_name().and_then(|name| name.to_str()) {
            Some(name) if name != ".cargo" && name != "target" => name,
            _ => continue,
        };

        if path.join("Cargo.toml").exists() {
            entries.push(name.to_string());
        }
    }

    entries.sort();
    entries.dedup();
    Ok(entries)
}

fn render_functions_workspace_manifest(exclude_entries: &[String]) -> String {
    let mut content = String::from(
        "# =============================================================================\n",
    );
    content.push_str("# Picus Functions Workspace\n");
    content.push_str("# Generic mixed-lane functions workspace grouped by domain.\n");
    content.push_str(
        "# =============================================================================\n\n",
    );
    content.push_str("[workspace]\n");
    content.push_str("resolver = \"2\"\n");
    content.push_str("members = [\n");
    content.push_str("]\n\n");
    content.push_str("exclude = [\n");

    for entry in exclude_entries {
        content.push_str(&format!("    \"{}\",\n", entry));
    }

    content.push_str("]\n");
    content
}

fn ensure_functions_workspace_bootstrap(
    functions_root: &Path,
    repo_root: &Path,
) -> Result<FunctionsWorkspaceBootstrap> {
    let mut bootstrap = FunctionsWorkspaceBootstrap::default();
    let cargo_toml_path = functions_root.join("Cargo.toml");

    if !cargo_toml_path.exists() {
        let mut exclude_entries = vec!["target".to_string()];
        let legacy_crates = flat_rust_function_dirs(functions_root)?;
        exclude_entries.extend(legacy_crates.iter().cloned());

        std::fs::write(
            &cargo_toml_path,
            render_functions_workspace_manifest(&exclude_entries),
        )
        .with_context(|| format!("Failed to create {}", cargo_toml_path.display()))?;

        bootstrap.created_manifest = true;
        bootstrap.excluded_legacy_crates = legacy_crates;
    }

    let cargo_config_dir = functions_root.join(".cargo");
    let cargo_config_path = cargo_config_dir.join("config.toml");
    if !cargo_config_path.exists() {
        std::fs::create_dir_all(&cargo_config_dir)
            .with_context(|| format!("Failed to create {}", cargo_config_dir.display()))?;

        let target_dir = relative_dependency_path(&cargo_config_dir, &repo_root.join("target"))?;
        let config_content = format!(
            "[build]\n# Reuse the repo-level host cache instead of a second functions-local target.\ntarget-dir = \"{}\"\n",
            target_dir
        );

        std::fs::write(&cargo_config_path, config_content)
            .with_context(|| format!("Failed to create {}", cargo_config_path.display()))?;
        bootstrap.created_build_config = true;
    }

    Ok(bootstrap)
}

fn report_functions_workspace_bootstrap(
    functions_root: &Path,
    bootstrap: &FunctionsWorkspaceBootstrap,
) {
    if bootstrap.created_manifest {
        println!(
            "✅ Created generic functions workspace manifest at {}",
            functions_root.join("Cargo.toml").display()
        );
    }

    if bootstrap.created_build_config {
        println!(
            "✅ Created shared target config at {}",
            functions_root.join(".cargo").join("config.toml").display()
        );
    }

    if !bootstrap.excluded_legacy_crates.is_empty() {
        println!(
            "ℹ️  Excluded legacy flat Rust crates from the new workspace: {}",
            bootstrap.excluded_legacy_crates.join(", ")
        );
    }
}

fn ensure_local_workspace_fence(cargo_toml_path: &Path) -> Result<bool> {
    let mut content = std::fs::read_to_string(cargo_toml_path)
        .with_context(|| format!("Failed to read {}", cargo_toml_path.display()))?;

    if content.contains("\n[workspace]\n") || content.starts_with("[workspace]\n") {
        return Ok(false);
    }

    if !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str("\n[workspace]\n");

    std::fs::write(cargo_toml_path, content)
        .with_context(|| format!("Failed to update {}", cargo_toml_path.display()))?;

    Ok(true)
}

fn validate_slug(value: &str, field_name: &str) -> Result<()> {
    if value.is_empty() {
        anyhow::bail!("{} must not be empty", field_name);
    }

    if !value
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        anyhow::bail!(
            "{} must contain only lowercase ASCII letters, digits, or '-'",
            field_name
        );
    }

    Ok(())
}

fn normalize_upload_language(value: &str) -> Result<&'static str> {
    match value.trim().to_ascii_lowercase().as_str() {
        "rust" | "rs" => Ok("rust"),
        "javascript" | "js" => Ok("javascript"),
        "python" | "py" => Ok("python"),
        other => anyhow::bail!(
            "Unsupported upload language '{}'. Supported: rust, javascript, python. AssemblyScript and TypeScript guest scaffolds have been removed; use the JavaScript lane authoring layout src/index.mjs and package it into the javascript-v1 bundle contract.",
            other
        ),
    }
}

/// Phase 4 slice A — assemble the artifact manifest and (optionally) sign
/// the artifact checksum so the control plane sees a complete attestation
/// envelope at publish time. The function reads the artifact bytes once and
/// returns a `PublishExtras` ready to be sent over multipart.
fn build_publish_extras(
    config: &PicusConfig,
    lang: Language,
    artifact_path: &Path,
    runtime_class_override: Option<String>,
    entrypoint_override: Option<String>,
    signing_key_path: Option<&Path>,
    signer_id: Option<&str>,
) -> Result<PublishExtras> {
    let artifact_bytes = std::fs::read(artifact_path).with_context(|| {
        format!(
            "Failed to read built artifact at {}",
            artifact_path.display()
        )
    })?;

    let language = match lang {
        Language::Rust => "rust",
        Language::JavaScript => "javascript",
        Language::Python => "python",
    };
    let default_runtime = match lang {
        Language::Rust => "native-v1",
        Language::JavaScript => "javascript-v1",
        Language::Python => "python-v1",
    };
    let runtime_class = runtime_class_override.unwrap_or_else(|| default_runtime.to_string());

    let entrypoint = entrypoint_override
        .or_else(|| config.function.entry_point.clone())
        .unwrap_or_else(|| "main".to_string());

    let manifest_doc = manifest::ArtifactManifest::new(
        &config.function.id,
        &config.function.name,
        language,
        &runtime_class,
        &entrypoint,
        &artifact_bytes,
    );
    let manifest_checksum = manifest_doc.checksum_hex()?;
    let allowed_capabilities = serde_json::to_string(&config.allowed_capabilities()?)
        .context("Failed to serialize capability manifest")?;

    // Pair check: signing key without signer_id (or vice versa) is a hard
    // error, so the operator does not accidentally publish unsigned thinking
    // they signed.
    let (signature, signer_id_out, signature_algorithm) = match (signing_key_path, signer_id) {
        (None, None) => (None, None, None),
        (Some(_), None) | (None, Some(_)) => {
            anyhow::bail!(
                "--signing-key-path and --signer-id must both be provided to sign a publish"
            );
        }
        (Some(key_path), Some(sid)) => {
            let sig = manifest::sign_artifact_checksum(key_path, &manifest_doc.artifact_checksum)?;
            println!(
                "🔏 Signed artifact checksum with signer '{}' using {}",
                sid,
                key_path.display()
            );
            (
                Some(sig),
                Some(sid.to_string()),
                Some("ed25519".to_string()),
            )
        }
    };

    Ok(PublishExtras {
        language: Some(language.to_string()),
        runtime_class: Some(runtime_class),
        entrypoint: Some(entrypoint),
        manifest_checksum: Some(manifest_checksum),
        allowed_capabilities: Some(allowed_capabilities),
        signature,
        signer_id: signer_id_out,
        signature_algorithm,
        sbom_checksum: None,
    })
}

fn activate_version_with_retry(
    client: &PicusClient,
    function_id: &str,
    version_id: &str,
) -> Result<()> {
    const MAX_ATTEMPTS: u32 = 30;

    for attempt in 1..=MAX_ATTEMPTS {
        match client.activate_version(version_id) {
            Ok(()) => return Ok(()),
            Err(error) => {
                let message = error.to_string();
                let warmup_pending =
                    message.contains("Warmup has not completed for this version yet");

                if !warmup_pending || attempt == MAX_ATTEMPTS {
                    return Err(error)
                        .with_context(|| format!("Activation failed for {}", function_id));
                }

                println!(
                    "⏳ Warmup not ready for '{}' (attempt {}/{}); retrying activation...",
                    function_id, attempt, MAX_ATTEMPTS
                );
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }

    unreachable!("activation retry loop exited unexpectedly")
}

pub(crate) fn deploy_function_dir(
    path: &Path,
    client_base_url: &str,
    cli_project_ref: Option<String>,
    runtime_class_override: Option<String>,
    entrypoint_override: Option<String>,
    signing_key_path: Option<PathBuf>,
    signer_id: Option<String>,
    personal_token: Option<String>,
) -> Result<()> {
    // We already know picus.toml exists if we reached here
    let config = PicusConfig::load_from_dir(path)?;
    println!(
        "🚀 Deploying function '{}' (ID: {})...",
        config.function.name, config.function.id
    );

    // Determine effective project_ref
    let project_ref = cli_project_ref.or(config.function.project_ref.clone());
    let client = PicusClient::new(client_base_url.to_string(), project_ref.clone(), None, None)
    .with_personal_token(personal_token);

    // Keep a single shared target under picus-erp/functions/target even when
    // functions are grouped one level deeper by domain.
    let absolute_target = shared_functions_target_dir(path);

    // Dispatch through the language adapter so all guest lanes share the same
    // deploy flow without duplicating logic here.
    let lang = detect_language(path).with_context(|| {
        format!(
            "Failed to detect guest language for function '{}'",
            config.function.id
        )
    })?;
    let adapter = adapter_for(lang);

    println!(
        "🛠️  Building function artifact via {} adapter...",
        adapter.language()
    );
    let build = adapter
        .build_with_runtime_class(
            path,
            true,
            &absolute_target,
            runtime_class_override.as_deref(),
        )
        .with_context(|| format!("Build failed for {}", config.function.id))?;
    let wasm_path = build.artifact;

    // Phase 4 slice A: build manifest + optional signature.
    let extras = build_publish_extras(
        &config,
        lang,
        &wasm_path,
        runtime_class_override,
        entrypoint_override,
        signing_key_path.as_deref(),
        signer_id.as_deref(),
    )?;

    // Upload
    println!("📤 Uploading {}...", wasm_path.display());
    let publish_result = client.upload_function_publish_result_with_extras(
        config.function.id.clone(),
        config.function.name.clone(),
        &wasm_path,
        &extras,
    )?;
    let func = publish_result.function;

    println!("✅ Function '{}' deployed successfully!", func.name);

    // Auto-upload OpenAPI fragment if openapi.yaml/yml/json exists alongside picus.toml
    // Precedence: openapi.yaml > openapi.yml > openapi.json
    let openapi_candidates = ["openapi.yaml", "openapi.yml", "openapi.json"];
    let openapi_file = openapi_candidates
        .iter()
        .map(|name| (name, path.join(name)))
        .find(|(_, p)| p.exists());

    if let Some((filename, openapi_path)) = openapi_file {
        if let Some(ref domain) = config.function.domain {
            let openapi_content = std::fs::read_to_string(&openapi_path)
                .with_context(|| format!("Failed to read {}", filename))?;

            let fragment: serde_json::Value =
                if filename.ends_with(".yaml") || filename.ends_with(".yml") {
                    let yaml_value: serde_json::Value = serde_yaml::from_str(&openapi_content)
                        .with_context(|| format!("Failed to parse {} as YAML", filename))?;
                    yaml_value
                } else {
                    serde_json::from_str(&openapi_content)
                        .with_context(|| format!("Failed to parse {} as JSON", filename))?
                };

            println!(
                "📋 Uploading OpenAPI fragment from {} for domain '{}'...",
                filename, domain
            );
            match client.upsert_openapi_fragment(
                config.function.id.clone(),
                domain.clone(),
                fragment.clone(),
            ) {
                Ok(_) => println!("✅ OpenAPI fragment registered for domain '{}'.", domain),
                Err(e) => eprintln!(
                    "⚠️  OpenAPI fragment upload failed: {}. Function deployment succeeded.",
                    e
                ),
            }

            // Auto-create route bindings from OpenAPI paths
            sync_route_bindings(&client, &config.function.id, &fragment, project_ref.as_deref());
        } else {
            eprintln!(
                "⚠️  {} found but no 'domain' in picus.toml; skipping OpenAPI registration.",
                filename
            );
        }
    }

    println!(
        "🚦 Activating published version {} for '{}'...",
        publish_result.version.id, config.function.id
    );
    activate_version_with_retry(&client, &config.function.id, &publish_result.version.id)?;
    println!("✅ Activated version {}", publish_result.version.id);

    Ok(())
}

/// Read OpenAPI paths and sync route bindings in the control plane.
/// Uses upsert (ON CONFLICT) instead of delete-recreate to preserve is_active state.
/// Reads x-picus-auth-mode and x-picus-required-permissions from OpenAPI operation extensions.
///
/// `x-picus-required-permissions` entries may contain the literal token `${PROJECT_REF}` (vd
/// `"entities:${PROJECT_REF}:warehouse:write"`) — thay bằng `project_ref` thật của lượt deploy
/// này trước khi gửi lên compute-control-plane. Cần thiết vì `permission_key` toàn cục
/// (`system_permission_catalog`) nhúng thẳng project_ref để tránh đụng giữa các project khác
/// nhau (xem `services/console-api/src/compute_proxy.rs::ensure_entity_permissions`), nên 1
/// bundle function triển khai lại cho project khác (`picus-erp` đã triển khai lên nhiều
/// project_ref khác nhau) mà hardcode sẵn 1 project_ref trong openapi.yaml sẽ luôn kiểm tra sai
/// quyền — permission_key đó không khớp bất kỳ role nào của project ĐANG deploy, dù người dùng
/// có đủ quyền thật. Không thay thế gì nếu thiếu `project_ref` (giữ nguyên chuỗi gốc, lỗi rõ hơn
/// còn hơn âm thầm im lặng).
fn sync_route_bindings(client: &PicusClient, function_key: &str, spec: &serde_json::Value, project_ref: Option<&str>) {
    // 1. Resolve function UUID from function_key
    let function_uuid = match client.list_compute_functions() {
        Ok(funcs) => {
            match funcs.iter().find(|f| f.function_key == function_key) {
                Some(f) => f.id.clone(),
                None => {
                    eprintln!("⚠️  Could not find compute function for key '{}'; skipping route bindings.", function_key);
                    return;
                }
            }
        }
        Err(e) => {
            eprintln!(
                "⚠️  Failed to list compute functions: {}; skipping route bindings.",
                e
            );
            return;
        }
    };

    // 2. Collect desired bindings from OpenAPI spec paths (with auth config)
    struct DesiredBinding {
        path_pattern: String,
        http_method: String,
        auth_mode: Option<String>,
        required_permissions: Option<Vec<String>>,
    }

    let mut desired: Vec<DesiredBinding> = Vec::new();
    if let Some(paths) = spec.get("paths").and_then(|p| p.as_object()) {
        for (path, methods) in paths {
            if let Some(methods_obj) = methods.as_object() {
                for (method, operation) in methods_obj {
                    let m = method.to_uppercase();
                    if matches!(
                        m.as_str(),
                        "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" | "OPTIONS"
                    ) {
                        // Read x-picus extensions from operation
                        let auth_mode = operation
                            .get("x-picus-auth-mode")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());

                        let required_permissions = operation
                            .get("x-picus-required-permissions")
                            .and_then(|v| v.as_array())
                            .map(|arr| {
                                arr.iter()
                                    .filter_map(|v| v.as_str())
                                    .map(|s| match project_ref {
                                        Some(p) => s.replace("${PROJECT_REF}", p),
                                        None => s.to_string(),
                                    })
                                    .collect()
                            });

                        desired.push(DesiredBinding {
                            path_pattern: path.clone(),
                            http_method: m,
                            auth_mode,
                            required_permissions,
                        });
                    }
                }
            }
        }
    }

    if desired.is_empty() {
        return;
    }

    // 3. Warn about stale bindings (openapi-managed bindings not in current spec)
    let desired_keys: std::collections::HashSet<(String, String)> = desired
        .iter()
        .map(|d| (d.path_pattern.clone(), d.http_method.clone()))
        .collect();

    match client.list_route_bindings() {
        Ok(bindings) => {
            for b in bindings
                .iter()
                .filter(|b| b.function_id == function_uuid && b.source == "openapi")
            {
                if !desired_keys.contains(&(b.path_pattern.clone(), b.http_method.clone())) {
                    eprintln!("⚠️  Stale route binding: {} {} (not in OpenAPI spec, source=openapi). Consider removing it.", b.http_method, b.path_pattern);
                }
            }
        }
        Err(e) => {
            eprintln!(
                "⚠️  Failed to list route bindings: {}; continuing with upsert.",
                e
            );
        }
    }

    // 4. Upsert route bindings (server handles ON CONFLICT)
    println!(
        "🔗 Syncing {} route binding(s) from OpenAPI paths...",
        desired.len()
    );
    for binding in &desired {
        let auth = binding.auth_mode.as_deref().unwrap_or("authenticated");
        match client.create_route_binding(
            binding.path_pattern.clone(),
            binding.http_method.clone(),
            function_uuid.clone(),
            Some(
                binding
                    .auth_mode
                    .clone()
                    .unwrap_or_else(|| "authenticated".to_string()),
            ),
            binding.required_permissions.clone(),
            Some("openapi".to_string()),
        ) {
            Ok(_) => println!(
                "   ✅ {} {} (auth_mode={})",
                binding.http_method, binding.path_pattern, auth
            ),
            Err(e) => eprintln!(
                "   ⚠️  {} {} failed: {}",
                binding.http_method, binding.path_pattern, e
            ),
        }
    }
}

fn is_newer_version(candidate: &str, current: &str) -> bool {
    fn parse(v: &str) -> (u32, u32, u32) {
        let p: Vec<&str> = v.split('.').collect();
        let n = |i: usize| p.get(i).and_then(|s| s.parse().ok()).unwrap_or(0);
        (n(0), n(1), n(2))
    }
    parse(candidate) > parse(current)
}

fn run_upgrade() -> Result<()> {
    let current = env!("CARGO_PKG_VERSION");
    println!("Current version: v{}", current);

    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let platform = match (os, arch) {
        ("linux", "x86_64") => "linux-x86_64",
        ("linux", "aarch64") => "linux-aarch64",
        ("macos", "x86_64") => "macos-x86_64",
        ("macos", "aarch64") => "macos-aarch64",
        _ => anyhow::bail!("Unsupported platform: {}-{}", os, arch),
    };

    println!("Checking latest release...");
    let http = reqwest::blocking::Client::builder()
        .user_agent("picusbase-cli")
        .timeout(Duration::from_secs(30))
        .build()?;
    let release: serde_json::Value = http
        .get("https://api.github.com/repos/picusbase/picusbase-cli/releases/latest")
        .send()
        .context("Failed to reach GitHub API")?
        .json()?;
    let tag = release["tag_name"]
        .as_str()
        .context("Unexpected GitHub API response")?;
    let latest = tag.trim_start_matches('v');

    if !is_newer_version(latest, current) {
        println!("Already up to date (v{}).", current);
        return Ok(());
    }
    println!("Updating v{} → v{}...", current, latest);

    let url = format!(
        "https://github.com/picusbase/picusbase-cli/releases/download/{}/picusbase-{}",
        tag, platform
    );
    let mut resp = http.get(&url).send().context("Download failed")?;
    if !resp.status().is_success() {
        anyhow::bail!("Download failed ({}): {}", resp.status(), url);
    }

    let current_exe = std::env::current_exe()?.canonicalize()?;
    // Write to a temp file in the same directory to allow same-filesystem rename
    let tmp_path = current_exe.with_file_name(".picusbase-upgrade-tmp");
    {
        let mut f = std::fs::File::create(&tmp_path).context("Cannot write to install directory")?;
        resp.copy_to(&mut f)?;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o755))?;
    }

    std::fs::rename(&tmp_path, &current_exe).map_err(|e| {
        let _ = std::fs::remove_file(&tmp_path);
        anyhow::anyhow!(
            "Failed to replace binary ({}). Try: sudo picusbase upgrade",
            e
        )
    })?;

    println!("Updated to v{}. Run `picusbase --version` to confirm.", latest);
    Ok(())
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let cli_defaults = CliDefaults::load()?;
    let explicit_profile_name = resolve_explicit_cli_profile(cli.profile.clone());
    let selected_profile_name = resolve_cli_profile(cli.profile.clone(), &cli_defaults);
    let skip_profile_resolution = matches!(
        &cli.command,
        Commands::Config(args)
            if matches!(
                args.command,
                ConfigCommands::Set { .. }
                    | ConfigCommands::Use { .. }
                    | ConfigCommands::DeleteProfile { .. }
                    | ConfigCommands::MigrateRootDefaults { .. }
            )
    );
    let selected_profile = if skip_profile_resolution {
        None
    } else {
        resolve_selected_profile(selected_profile_name.as_deref(), &cli_defaults)?
    };
    let resolved_url = resolve_cli_url(cli.url.clone(), &cli_defaults, selected_profile);
    let resolved_project_ref =
        resolve_cli_project_ref(cli.project_ref.clone(), &cli_defaults, selected_profile);
    let resolved_executor_url =
        resolve_cli_executor_url(cli.executor_url.clone(), &cli_defaults, selected_profile);
    let resolved_auth_token = resolve_cli_auth_token(cli.auth_token.clone());
    let resolved_personal_token = resolve_cli_personal_token(&cli_defaults, selected_profile);
    let client = PicusClient::new(
        resolved_url.clone(),
        resolved_project_ref.clone(),
        resolved_executor_url.clone(),
        resolved_auth_token.clone(),
    )
    .with_personal_token(resolved_personal_token.clone());

    match cli.command {
        Commands::Config(args) => match args.command {
            ConfigCommands::Show => {
                let path = CliDefaults::path()?;
                println!("Config file: {}", path.display());
                println!(
                    "Default profile: {}",
                    cli_defaults.default_profile.as_deref().unwrap_or("<unset>")
                );
                println!("Stored root defaults:");
                println!(
                    "  url = {}",
                    cli_defaults.url.as_deref().unwrap_or("<unset>")
                );
                println!(
                    "  executor_url = {}",
                    cli_defaults.executor_url.as_deref().unwrap_or("<unset>")
                );
                println!(
                    "  project_ref = {}",
                    cli_defaults.project_ref.as_deref().unwrap_or("<unset>")
                );
                println!(
                    "  personal_token = {}",
                    token_state(&cli_defaults.personal_token)
                );
                println!("Profiles:");
                if cli_defaults.profiles.is_empty() {
                    println!("  <none>");
                } else {
                    for (name, profile) in &cli_defaults.profiles {
                        let mut markers = Vec::new();
                        if cli_defaults.default_profile.as_deref() == Some(name.as_str()) {
                            markers.push("default");
                        }
                        if selected_profile_name.as_deref() == Some(name.as_str()) {
                            markers.push("selected");
                        }
                        if explicit_profile_name.as_deref() == Some(name.as_str()) {
                            markers.push("explicit");
                        }

                        if markers.is_empty() {
                            println!("  - {}", name);
                        } else {
                            println!("  - {} ({})", name, markers.join(", "));
                        }
                        println!(
                            "      url = {}",
                            profile.url.as_deref().unwrap_or("<unset>")
                        );
                        println!(
                            "      executor_url = {}",
                            profile.executor_url.as_deref().unwrap_or("<unset>")
                        );
                        println!(
                            "      project_ref = {}",
                            profile.project_ref.as_deref().unwrap_or("<unset>")
                        );
                        println!(
                            "      personal_token = {}",
                            token_state(&profile.personal_token)
                        );
                    }
                }
                println!(
                    "Selected profile: {}",
                    selected_profile_name.as_deref().unwrap_or("<unset>")
                );
                println!("Effective values:");
                println!("  url = {}", resolved_url);
                println!(
                    "  executor_url = {}",
                    resolved_executor_url.as_deref().unwrap_or("<unset>")
                );
                println!(
                    "  project_ref = {}",
                    resolved_project_ref.as_deref().unwrap_or("<unset>")
                );
                println!(
                    "  personal_token = {}",
                    token_state(&resolved_personal_token)
                );
                Ok(())
            }
            ConfigCommands::Set {
                default_url,
                default_executor_url,
                default_project_ref,
                personal_token,
            } => {
                let target_profile_name = explicit_profile_name.clone();
                if default_url.is_none()
                    && default_executor_url.is_none()
                    && default_project_ref.is_none()
                    && personal_token.is_none()
                {
                    anyhow::bail!(
                        "Provide at least one of --default-url, --default-executor-url, --default-project-ref, or --personal-token"
                    );
                }

                let mut next = cli_defaults.clone();

                if let Some(profile_name) = target_profile_name.as_deref() {
                    let profile = next.upsert_profile(profile_name);

                    if let Some(default_url) = default_url {
                        profile.url = normalize_optional_string(Some(default_url));
                    }

                    if let Some(default_executor_url) = default_executor_url {
                        profile.executor_url =
                            normalize_optional_string(Some(default_executor_url));
                    }

                    if let Some(default_project_ref) = default_project_ref {
                        profile.project_ref = normalize_optional_string(Some(default_project_ref));
                    }

                    if let Some(personal_token) = personal_token {
                        profile.personal_token = normalize_optional_string(Some(personal_token));
                    }
                } else {
                    if let Some(default_url) = default_url {
                        next.url = normalize_optional_string(Some(default_url));
                    }

                    if let Some(default_executor_url) = default_executor_url {
                        next.executor_url = normalize_optional_string(Some(default_executor_url));
                    }

                    if let Some(default_project_ref) = default_project_ref {
                        next.project_ref = normalize_optional_string(Some(default_project_ref));
                    }

                    if let Some(personal_token) = personal_token {
                        next.personal_token = normalize_optional_string(Some(personal_token));
                    }
                }

                let path = next.save()?;
                println!("Saved CLI defaults to {}", path.display());
                if let Some(profile_name) = target_profile_name {
                    let profile = next
                        .profiles
                        .get(&profile_name)
                        .expect("profile must exist after set");
                    println!("Updated profile '{}':", profile_name);
                    println!("  url = {}", profile.url.as_deref().unwrap_or("<unset>"));
                    println!(
                        "  executor_url = {}",
                        profile.executor_url.as_deref().unwrap_or("<unset>")
                    );
                    println!(
                        "  project_ref = {}",
                        profile.project_ref.as_deref().unwrap_or("<unset>")
                    );
                    println!("  personal_token = {}", token_state(&profile.personal_token));
                } else {
                    println!("Updated root defaults:");
                    println!("  url = {}", next.url.as_deref().unwrap_or("<unset>"));
                    println!(
                        "  executor_url = {}",
                        next.executor_url.as_deref().unwrap_or("<unset>")
                    );
                    println!(
                        "  project_ref = {}",
                        next.project_ref.as_deref().unwrap_or("<unset>")
                    );
                    println!("  personal_token = {}", token_state(&next.personal_token));
                }
                Ok(())
            }
            ConfigCommands::Use { profile, clear } => {
                if clear && profile.is_some() {
                    anyhow::bail!("Provide either a profile name or --clear, not both");
                }

                if !clear && profile.is_none() {
                    anyhow::bail!("Provide a profile name or use --clear");
                }

                let mut next = cli_defaults.clone();

                if clear {
                    next.default_profile = None;
                    let path = next.save()?;
                    println!("Saved CLI defaults to {}", path.display());
                    println!(
                        "Default profile cleared; CLI will fall back to stored root defaults."
                    );
                    return Ok(());
                }

                let profile_name =
                    normalize_optional_string(profile).expect("validated profile name must exist");
                if !next.profiles.contains_key(&profile_name) {
                    anyhow::bail!(
                        "Profile '{}' was not found. Create it first with `picusbase config set --profile {}`.",
                        profile_name,
                        profile_name
                    );
                }

                next.default_profile = Some(profile_name.clone());
                let path = next.save()?;
                println!("Saved CLI defaults to {}", path.display());
                println!("Default profile = {}", profile_name);
                Ok(())
            }
            ConfigCommands::DeleteProfile { profile } => {
                let profile_name = normalize_optional_string(Some(profile))
                    .context("Profile name must not be empty")?;
                let mut next = cli_defaults.clone();

                if next.profiles.remove(&profile_name).is_none() {
                    anyhow::bail!(
                        "Profile '{}' was not found. Use `picusbase config show` to inspect available profiles.",
                        profile_name
                    );
                }

                if next.default_profile.as_deref() == Some(profile_name.as_str()) {
                    next.default_profile = None;
                }

                let path = next.save()?;
                println!("Saved CLI defaults to {}", path.display());
                println!("Deleted profile '{}'.", profile_name);
                if next.default_profile.is_none() {
                    println!("Default profile = <unset>");
                } else {
                    println!(
                        "Default profile = {}",
                        next.default_profile.as_deref().unwrap_or("<unset>")
                    );
                }
                Ok(())
            }
            ConfigCommands::MigrateRootDefaults { profile } => {
                let profile_name = normalize_optional_string(Some(profile))
                    .context("Profile name must not be empty")?;
                let mut next = cli_defaults.clone();

                let root_url = next.url.clone();
                let root_executor_url = next.executor_url.clone();
                let root_project_ref = next.project_ref.clone();
                let root_personal_token = next.personal_token.clone();

                if root_url.is_none()
                    && root_executor_url.is_none()
                    && root_project_ref.is_none()
                    && root_personal_token.is_none()
                {
                    anyhow::bail!("Root defaults are already empty. Nothing to migrate.");
                }

                {
                    let profile = next.upsert_profile(&profile_name);
                    if root_url.is_some() {
                        profile.url = root_url.clone();
                    }
                    if root_executor_url.is_some() {
                        profile.executor_url = root_executor_url.clone();
                    }
                    if root_project_ref.is_some() {
                        profile.project_ref = root_project_ref.clone();
                    }
                    if root_personal_token.is_some() {
                        profile.personal_token = root_personal_token.clone();
                    }
                }

                next.url = None;
                next.executor_url = None;
                next.project_ref = None;
                next.personal_token = None;
                next.default_profile = Some(profile_name.clone());

                let path = next.save()?;
                let profile = next
                    .profiles
                    .get(&profile_name)
                    .expect("profile must exist after migrating root defaults");
                println!("Saved CLI defaults to {}", path.display());
                println!(
                    "Migrated root defaults into profile '{}' and set it as the default profile.",
                    profile_name
                );
                println!("  url = {}", profile.url.as_deref().unwrap_or("<unset>"));
                println!(
                    "  executor_url = {}",
                    profile.executor_url.as_deref().unwrap_or("<unset>")
                );
                println!(
                    "  project_ref = {}",
                    profile.project_ref.as_deref().unwrap_or("<unset>")
                );
                Ok(())
            }
        },
        Commands::Domain(args) => {
            let project_ref = resolved_project_ref.as_deref();
            match args.command {
                DomainCommands::List => web_app::list_domains(&client, project_ref),
                DomainCommands::Set { app_code, domain } => web_app::set_domain(&client, project_ref, &app_code, &domain),
                DomainCommands::Remove { app_code } => web_app::remove_domain(&client, project_ref, &app_code),
            }
        }
        Commands::Gen(args) => match args.command {
            GenCommands::Types { path, out } => gen_types::run(&path, &out),
        },
        Commands::Data(args) => match args.command {
            DataCommands::Restore { dir } => {
                let project_ref = resolved_project_ref.as_deref().context("project_ref is required: --project-ref or PICUS_PROJECT_REF")?;
                schema_deploy::restore_data(&client, &dir, project_ref)
            }
        },
        Commands::Deploy(args) => match args.command {
            DeployCommands::Schema { path, skip_functions, no_publish, bound_user_id, dry_run, keep_going } => {
                let project_ref = resolved_project_ref.as_deref().context("project_ref is required: --project-ref or PICUS_PROJECT_REF")?;
                schema_deploy::run(
                    &client,
                    &path,
                    project_ref,
                    resolved_personal_token.clone(),
                    schema_deploy::SchemaDeployOptions { skip_functions, no_publish, bound_user_id, dry_run, keep_going },
                )
            }
            DeployCommands::App { path, domain, skip_build } => {
                // Explicit --project-ref / PICUS_PROJECT_REF beats picus.app.toml, which beats the profile.
                let explicit_project_ref = normalize_optional_string(cli.project_ref.clone())
                    .or_else(|| normalize_optional_string(std::env::var("PICUS_PROJECT_REF").ok()));
                web_app::deploy_app(
                    &client,
                    explicit_project_ref.as_deref(),
                    resolved_project_ref.as_deref(),
                    &path,
                    domain,
                    skip_build,
                )
            }
            DeployCommands::Functions {
                path,
                ids,
                rebuild: _,
                runtime_class,
                entrypoint,
                signing_key_path,
                signer_id,
            } => {
                // Case 1: If run in a single function folder (has picus.toml)
                // If path points to single function (explicit or default .)
                if path.join("picus.toml").exists() {
                    let config = PicusConfig::load_from_dir(&path)?;
                    if !ids.is_empty() && !ids.contains(&config.function.id) {
                        println!("⚠️  Current directory function '{}' does not match requested IDs {:?}. Skipping.", config.function.id, ids);
                        return Ok(());
                    }
                    return deploy_function_dir(
                        &path,
                        &resolved_url,
                        resolved_project_ref,
                        runtime_class,
                        entrypoint,
                        signing_key_path,
                        signer_id,
                        resolved_personal_token,
                    );
                }

                // Case 2: Run in parent folder and recursively discover function crates.
                println!("📂 Scanning for functions in {:?}...", path);
                let mut functions_discovered = 0;
                let mut functions_found = 0;
                let mut functions_failed = 0;

                let mut function_dirs = Vec::new();
                collect_function_dirs(&path, &mut function_dirs)?;

                for function_dir in function_dirs {
                    let config = match PicusConfig::load_from_dir(&function_dir) {
                        Ok(c) => c,
                        Err(_) => continue,
                    };

                    if ids.is_empty() || ids.contains(&config.function.id) {
                        functions_discovered += 1;
                        println!("--------------------------------------------------");
                        if let Err(e) = deploy_function_dir(
                            &function_dir,
                            &resolved_url,
                            resolved_project_ref.clone(),
                            runtime_class.clone(),
                            entrypoint.clone(),
                            signing_key_path.clone(),
                            signer_id.clone(),
                            resolved_personal_token.clone(),
                        ) {
                            eprintln!("❌ Failed to deploy {}: {}", config.function.id, e);
                            functions_failed += 1;
                        } else {
                            functions_found += 1;
                        }
                    }
                }

                if functions_discovered == 0 {
                    if !ids.is_empty() {
                        println!("⚠️  No matching functions found for IDs: {:?}", ids);
                    } else {
                        println!("⚠️  No functions found in current directory.");
                    }
                } else {
                    println!("--------------------------------------------------");
                    if functions_failed == 0 {
                        println!("✅ Deployed {} functions.", functions_found);
                    } else {
                        println!(
                            "⚠️  Deployment finished: {} succeeded, {} failed.",
                            functions_found, functions_failed
                        );
                    }
                }

                Ok(())
            }
        },
        Commands::Function(args) => {
            match args.command {
                FunctionCommands::List => {
                    let functions = client.list_functions()?;
                    println!("{}", Table::new(functions).to_string());
                }
                FunctionCommands::Build { path, release } => {
                    println!("Building function artifact in {:?}...", path);

                    let absolute_target = shared_functions_target_dir(&path);
                    let lang = detect_language(&path)?;
                    let adapter = adapter_for(lang);

                    println!("Using {} adapter.", adapter.language());
                    let build = adapter.build(&path, release, &absolute_target)?;
                    println!("Build successful!");
                    println!("Artifact: {}", build.artifact.display());
                }
                FunctionCommands::Upload {
                    id,
                    name,
                    file,
                    runtime_class,
                    entrypoint,
                    signing_key_path,
                    signer_id,
                    language,
                } => {
                    let name = name.unwrap_or_else(|| id.clone());
                    let artifact_bytes = std::fs::read(&file).with_context(|| {
                        format!("Failed to read artifact at {}", file.display())
                    })?;
                    let language = normalize_upload_language(&language)?;
                    let runtime_class = runtime_class.unwrap_or_else(|| match language {
                        "javascript" => "javascript-v1".to_string(),
                        "python" => "python-v1".to_string(),
                        _ => "wasm-core-v1".to_string(),
                    });
                    let entrypoint = entrypoint.unwrap_or_else(|| "main".to_string());
                    let manifest_doc = manifest::ArtifactManifest::new(
                        &id,
                        &name,
                        language,
                        &runtime_class,
                        &entrypoint,
                        &artifact_bytes,
                    );
                    let manifest_checksum = manifest_doc.checksum_hex()?;
                    let (signature, signer_id_val, signature_algorithm) = match (
                        signing_key_path.as_deref(),
                        signer_id.as_deref(),
                    ) {
                        (None, None) => (None, None, None),
                        (Some(_), None) | (None, Some(_)) => {
                            anyhow::bail!(
                                    "--signing-key-path and --signer-id must both be provided to sign a publish"
                                );
                        }
                        (Some(key_path), Some(sid)) => {
                            let sig = manifest::sign_artifact_checksum(
                                key_path,
                                &manifest_doc.artifact_checksum,
                            )?;
                            (
                                Some(sig),
                                Some(sid.to_string()),
                                Some("ed25519".to_string()),
                            )
                        }
                    };
                    let extras = PublishExtras {
                        language: Some(language.to_string()),
                        runtime_class: Some(runtime_class),
                        entrypoint: Some(entrypoint),
                        manifest_checksum: Some(manifest_checksum),
                        allowed_capabilities: Some("[]".to_string()),
                        signature,
                        signer_id: signer_id_val,
                        signature_algorithm,
                        sbom_checksum: None,
                    };
                    let func = client.upload_function_with_extras(id, name, &file, &extras)?;
                    println!("Function uploaded successfully:");
                    println!("{}", Table::new(vec![func]).to_string());
                }
                FunctionCommands::Run { id, params } => {
                    let params_json: serde_json::Value = serde_json::from_str(&params)?;
                    let result = client.run_function(&id, params_json)?;
                    println!("Execution Result:");
                    println!("{}", serde_json::to_string_pretty(&result)?);
                }
                FunctionCommands::Delete { id } => {
                    if client.delete_function(&resolve_function_uuid(&client, &id)?)? {
                        println!("Function '{}' deleted successfully.", id);
                    } else {
                        println!("Failed to delete function '{}'.", id);
                    }
                }
            }
            Ok(())
        }
        Commands::Api(args) => {
            match args.command {
            ApiCommands::List { method, filter, json } => {
                let ops = api::list(method.as_deref(), filter.as_deref())?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&ops)?);
                } else {
                    let rows: Vec<api::Row> = ops.into_iter().map(api::Row::from).collect();
                    println!("{}", Table::new(rows));
                }
            }
            ApiCommands::Describe { operation_id } => {
                println!("{}", serde_json::to_string_pretty(&api::describe(&operation_id)?)?);
            }
            ApiCommands::Call { method, path, query, data, data_file, yes } => {
                let body: Option<serde_json::Value> = match (data, data_file) {
                    (Some(text), _) => Some(serde_json::from_str(&text).context("--data is not valid JSON")?),
                    (None, Some(file)) => {
                        let text = std::fs::read_to_string(&file)
                            .with_context(|| format!("reading {}", file.display()))?;
                        Some(serde_json::from_str(&text).context("--data-file is not valid JSON")?)
                    }
                    (None, None) => None,
                };
                let (op, full_path, query_string) =
                    api::resolve(&method, &path, resolved_project_ref.as_deref(), &query)?;
                let target = if query_string.is_empty() {
                    full_path
                } else {
                    format!("{full_path}?{query_string}")
                };
                if op.method != "GET" && !yes {
                    eprintln!(
                        "Not sent: {} {} ({}; permissions: {}). Re-run with --yes to send.",
                        op.method,
                        target,
                        op.operation_id,
                        op.permissions.join(" ")
                    );
                    std::process::exit(2);
                }
                let (status, text) = client.send_api(&op.method, &target, body.as_ref())?;
                if (200..300).contains(&status) {
                    match serde_json::from_str::<serde_json::Value>(&text) {
                        Ok(value) => println!("{}", serde_json::to_string_pretty(&value)?),
                        Err(_) => println!("{text}"),
                    }
                } else {
                    anyhow::bail!("HTTP {status}: {text}");
                }
            }
            }
            Ok(())
        }
        Commands::Job(args) => {
            match args.command {
                JobCommands::List => {
                    let jobs = client.list_jobs()?;
                    println!("{}", Table::new(jobs).to_string());
                }
                JobCommands::Create {
                    function_id,
                    params,
                } => {
                    let params_json: serde_json::Value = serde_json::from_str(&params)?;
                    let job = client.create_job(function_id, params_json)?;
                    println!("Job created successfully:");
                    println!("{}", Table::new(vec![job]).to_string());
                }
                JobCommands::Delete { id } => {
                    if client.delete_job(&id)? {
                        println!("Job '{}' deleted successfully.", id);
                    } else {
                        println!("Failed to delete job '{}'.", id);
                    }
                }
            }
            Ok(())
        }
        Commands::Route(args) => {
            match args.command {
                RouteCommands::List => {
                    let bindings = client.list_route_bindings()?;
                    println!("{}", Table::new(bindings).to_string());
                }
                RouteCommands::Bind {
                    function,
                    path,
                    method,
                    auth_mode,
                    permissions,
                } => {
                    let binding = client.create_route_binding(
                        path,
                        method.to_uppercase(),
                        resolve_function_uuid(&client, &function)?,
                        Some(auth_mode),
                        (!permissions.is_empty()).then_some(permissions),
                        Some("manual".to_string()),
                    )?;
                    println!("{}", Table::new(vec![binding]).to_string());
                }
                RouteCommands::Delete { id } => {
                    client.delete_route_binding(&id)?;
                    println!("Route binding '{}' deleted.", id);
                }
            }
            Ok(())
        }
        Commands::Schedule(args) => {
            match args.command {
                ScheduleCommands::List => {
                    let schedules = client.list_schedules()?;
                    println!("{}", Table::new(schedules).to_string());
                }
                ScheduleCommands::Create {
                    name,
                    cron,
                    function_id,
                    params,
                } => {
                    let params_json: serde_json::Value = serde_json::from_str(&params)?;
                    let id = client.create_schedule(name, cron, function_id, params_json)?;
                    println!("Schedule created successfully. ID: {}", id);
                }
                ScheduleCommands::Delete { id } => {
                    if client.delete_schedule(&id)? {
                        println!("Schedule '{}' deleted successfully.", id);
                    } else {
                        println!("Failed to delete schedule '{}'.", id);
                    }
                }
            }
            Ok(())
        }
        Commands::Init(args) => match args.command {
            InitCommands::App { code, framework, name, domain, dir } => {
                let name = name.unwrap_or_else(|| title_case(&code));
                web_app::init_app(&client, resolved_project_ref.as_deref(), &code, &framework, &name, domain, dir)
            }
            InitCommands::Functions {
                domain,
                function_id,
                name,
                id,
                project_ref,
                no_workspace_member,
                language,
            } => {
                let lang = Language::parse(&language)?;

                println!("Initializing new function scaffold (language: {})...", lang);

                let current_dir = std::env::current_dir()?;
                let repo_root = find_repo_root(&current_dir);
                let functions_root = find_managed_functions_root(&current_dir);
                let inferred_domain = functions_root
                    .as_ref()
                    .and_then(|root| infer_domain_from_path(&current_dir, root));

                let function_id = match (function_id, id) {
                    (Some(positional), Some(flag)) if positional != flag => {
                        anyhow::bail!(
                            "Conflicting function IDs provided: '{}' and '{}'",
                            positional,
                            flag
                        );
                    }
                    (Some(positional), _) => positional,
                    (None, Some(flag)) => flag,
                    (None, None) => prompt_with_default("Function ID (slug)", "my-function")?,
                };
                validate_slug(&function_id, "Function ID")?;

                let domain = match (domain, inferred_domain.clone()) {
                    (Some(explicit), Some(inferred)) if explicit != inferred => {
                        anyhow::bail!(
                            "Current directory is inside domain '{}', but '--domain' resolves to '{}'. Omit the domain flag here or run the command from the target parent directory.",
                            inferred,
                            explicit
                        );
                    }
                    (Some(explicit), _) => explicit,
                    (None, Some(inferred)) => inferred,
                    (None, None) => prompt_with_default("Domain", "inventory")?,
                };
                validate_slug(&domain, "Domain")?;

                let function_name = name.unwrap_or_else(|| title_case(&function_id));

                let project_ref = project_ref
                    .or_else(|| resolved_project_ref.clone())
                    .or_else(|| {
                        functions_root
                            .as_ref()
                            .and_then(|root| infer_project_ref(root, &domain))
                    })
                    .unwrap_or_else(|| {
                        if functions_root.is_some() {
                            "p_8308d170".to_string()
                        } else {
                            "p_XXXXXXXX".to_string()
                        }
                    });

                let function_dir =
                    scaffold_parent_dir(&current_dir, functions_root.as_deref(), &domain)
                        .join(&function_id);

                if function_dir.exists() {
                    anyhow::bail!(
                        "Function directory already exists: {}",
                        function_dir.display()
                    );
                }

                let schema_prefix = pascal_case(&function_id);

                if lang == Language::Python {
                    // ----- Python (CPython source bundle) scaffold -----
                    // Authoring contract: docs-platform/01-architecture/04-compute-architecture.md
                    // The function dir keeps metadata at the root and
                    // Python source under src/, so the authoring layout
                    // matches Rust and JavaScript. The
                    // CLI's PythonAdapter stages the launcher crate at
                    // build time; no per-function Cargo.toml is created.
                    std::fs::create_dir_all(function_dir.join("src"))?;
                    println!("📂 Created directory: {}", function_dir.display());

                    let picus_toml_content = include_str!("templates/python/picus.toml")
                        .replace("{{FUNCTION_ID}}", &function_id)
                        .replace("{{FUNCTION_NAME}}", &function_name)
                        .replace("{{PROJECT_REF}}", &project_ref)
                        .replace("{{FUNCTION_DOMAIN}}", &domain);
                    std::fs::write(function_dir.join("picus.toml"), picus_toml_content)?;
                    println!("✅ Created picus.toml");

                    let handler_py_content = include_str!("templates/python/handler.py")
                        .replace("{{FUNCTION_ID}}", &function_id)
                        .replace("{{FUNCTION_NAME}}", &function_name);
                    std::fs::write(
                        function_dir.join("src").join("handler.py"),
                        handler_py_content,
                    )?;
                    println!("✅ Created src/handler.py");

                    let pyproject_content = include_str!("templates/python/pyproject.toml")
                        .replace("{{FUNCTION_ID}}", &function_id)
                        .replace("{{FUNCTION_NAME}}", &function_name);
                    std::fs::write(function_dir.join("pyproject.toml"), pyproject_content)?;
                    println!("✅ Created pyproject.toml");

                    let openapi_content = include_str!("templates/openapi.yaml")
                        .replace("{{FUNCTION_ID}}", &function_id)
                        .replace("{{FUNCTION_NAME}}", &function_name)
                        .replace("{{DOMAIN_TAG}}", &title_case(&domain))
                        .replace("{{SCHEMA_PREFIX}}", &schema_prefix);
                    std::fs::write(function_dir.join("openapi.yaml"), openapi_content)?;
                    println!("✅ Created openapi.yaml");

                    println!("\n🎉 Function initialized at {}", function_dir.display());
                    println!("Next steps:");
                    println!("  1. Implement handle(request, context) in src/handler.py");
                    println!("  2. Refine request/response schemas in openapi.yaml");
                    println!(
                        "  3. Build: cd {} && picusbase function build --path . --release",
                        function_dir.display()
                    );
                    println!(
                        "  4. Deploy: cd {} && picusbase deploy functions --rebuild",
                        function_dir.display()
                    );
                    println!(
                        "  Note: Python functions are NOT cargo workspace members; the source bundle is produced by PythonAdapter via `picusbase bundle python`."
                    );

                    // Ensure the Python function dir is excluded from
                    // the Rust functions workspace so `cargo metadata`
                    // does not error on a member without Cargo.toml.
                    if !no_workspace_member {
                        if let Some(root) = functions_root.as_ref() {
                            let workspace_repo_root = repo_root.as_deref().ok_or_else(|| {
                                anyhow::anyhow!(
                                    "Managed functions workspace bootstrap requires running inside the Picus monorepo."
                                )
                            })?;
                            let bootstrap =
                                ensure_functions_workspace_bootstrap(root, workspace_repo_root)?;
                            report_functions_workspace_bootstrap(root, &bootstrap);

                            let exclude_entry = format!("{}/{}", domain, function_id);
                            if add_functions_workspace_exclude_entry(root, &exclude_entry)? {
                                println!(
                                    "✅ Added {} to functions workspace exclude list",
                                    exclude_entry
                                );
                            }
                        } else {
                            println!(
                                "ℹ️  Current directory is outside a managed functions root; skipped functions workspace update."
                            );
                        }
                    }

                    return Ok(());
                }

                if lang == Language::JavaScript {
                    // ----- JavaScript bundle scaffold -----
                    std::fs::create_dir_all(function_dir.join("src"))?;
                    println!("📂 Created directory: {}", function_dir.display());

                    let picus_toml_content = format!(
                        "{}\nlanguage = \"javascript\"\n",
                        include_str!("templates/picus.toml")
                            .replace("{{FUNCTION_ID}}", &function_id)
                            .replace("{{FUNCTION_NAME}}", &function_name)
                            .replace("{{PROJECT_REF}}", &project_ref)
                            .replace("{{FUNCTION_DOMAIN}}", &domain)
                    );
                    std::fs::write(function_dir.join("picus.toml"), picus_toml_content)?;
                    println!("✅ Created picus.toml");

                    let index_mjs_content = include_str!("templates/javascript/index.mjs")
                        .replace("{{FUNCTION_ID}}", &function_id)
                        .replace("{{FUNCTION_NAME}}", &function_name);
                    std::fs::write(
                        function_dir.join("src").join("index.mjs"),
                        index_mjs_content,
                    )?;
                    println!("✅ Created src/index.mjs");

                    let openapi_content = include_str!("templates/openapi.yaml")
                        .replace("{{FUNCTION_ID}}", &function_id)
                        .replace("{{FUNCTION_NAME}}", &function_name)
                        .replace("{{DOMAIN_TAG}}", &title_case(&domain))
                        .replace("{{SCHEMA_PREFIX}}", &schema_prefix);
                    std::fs::write(function_dir.join("openapi.yaml"), openapi_content)?;
                    println!("✅ Created openapi.yaml");

                    println!("\n🎉 Function initialized at {}", function_dir.display());
                    println!("Next steps:");
                    println!("  1. Implement the handler in src/index.mjs");
                    println!("  2. Refine request/response schemas in openapi.yaml");
                    println!(
                        "  3. Validate/package: cd {} && picusbase function build --release",
                        function_dir.display()
                    );
                    println!(
                        "  4. Deploy: cd {} && picusbase deploy functions --rebuild",
                        function_dir.display()
                    );

                    if !no_workspace_member {
                        if let Some(root) = functions_root.as_ref() {
                            let workspace_repo_root = repo_root.as_deref().ok_or_else(|| {
                                anyhow::anyhow!(
                                    "Managed functions workspace bootstrap requires running inside the Picus monorepo."
                                )
                            })?;
                            let bootstrap =
                                ensure_functions_workspace_bootstrap(root, workspace_repo_root)?;
                            report_functions_workspace_bootstrap(root, &bootstrap);

                            let exclude_entry = format!("{}/{}", domain, function_id);
                            if add_functions_workspace_exclude_entry(root, &exclude_entry)? {
                                println!(
                                    "✅ Added {} to functions workspace exclude list",
                                    exclude_entry
                                );
                            }
                        } else {
                            println!(
                                "ℹ️  Current directory is outside a managed functions root; skipped functions workspace update."
                            );
                        }
                    }

                    return Ok(());
                }

                std::fs::create_dir_all(function_dir.join("src"))?;
                println!("📂 Created directory: {}", function_dir.display());

                let rust_repo_root = repo_root.ok_or_else(|| {
                    anyhow::anyhow!(
                        "Rust scaffold requires running inside the Picus monorepo so dependency paths can be generated."
                    )
                })?;
                let (function_kit_path, wasm_sdk_path) =
                    rust_scaffold_dependency_paths(&rust_repo_root, &function_dir)?;
                let optional_dependencies =
                    optional_function_dependencies(Some(&rust_repo_root), &function_dir, &domain)?;

                let picus_toml_content = include_str!("templates/picus.toml")
                    .replace("{{FUNCTION_ID}}", &function_id)
                    .replace("{{FUNCTION_NAME}}", &function_name)
                    .replace("{{PROJECT_REF}}", &project_ref)
                    .replace("{{FUNCTION_DOMAIN}}", &domain);
                std::fs::write(function_dir.join("picus.toml"), picus_toml_content)?;
                println!("✅ Created picus.toml");

                let cargo_toml_content = include_str!("templates/rust/Cargo.toml")
                    .replace("{{FUNCTION_ID}}", &function_id)
                    .replace("{{FUNCTION_KIT_PATH}}", &function_kit_path)
                    .replace("{{WASM_SDK_PATH}}", &wasm_sdk_path)
                    .replace("{{OPTIONAL_DEPENDENCIES}}", &optional_dependencies);
                let cargo_toml_path = function_dir.join("Cargo.toml");
                std::fs::write(&cargo_toml_path, cargo_toml_content)?;
                println!("✅ Created Cargo.toml");

                let lib_rs_content = include_str!("templates/rust/lib.rs")
                    .replace("{{STRUCT_NAME}}", &schema_prefix)
                    .replace("{{FUNCTION_ID}}", &function_id);
                std::fs::write(function_dir.join("src").join("lib.rs"), lib_rs_content)?;
                println!("✅ Created src/lib.rs");

                let openapi_content = include_str!("templates/openapi.yaml")
                    .replace("{{FUNCTION_ID}}", &function_id)
                    .replace("{{FUNCTION_NAME}}", &function_name)
                    .replace("{{DOMAIN_TAG}}", &title_case(&domain))
                    .replace("{{SCHEMA_PREFIX}}", &schema_prefix);
                std::fs::write(function_dir.join("openapi.yaml"), openapi_content)?;
                println!("✅ Created openapi.yaml");

                if let Some(root) = functions_root.as_ref() {
                    let bootstrap = ensure_functions_workspace_bootstrap(root, &rust_repo_root)?;
                    report_functions_workspace_bootstrap(root, &bootstrap);

                    if !no_workspace_member {
                        if add_functions_workspace_domain(root, &domain)? {
                            println!("✅ Updated functions workspace for domain: {}", domain);
                        } else {
                            println!("ℹ️  Functions workspace already covers domain: {}", domain);
                        }
                    } else {
                        let exclude_entry = format!("{}/{}", domain, function_id);
                        if add_functions_workspace_exclude_entry(root, &exclude_entry)? {
                            println!(
                                "✅ Added {} to functions workspace exclude list",
                                exclude_entry
                            );
                        } else {
                            println!("ℹ️  Functions workspace already excludes {}", exclude_entry);
                        }
                    }
                } else {
                    if ensure_local_workspace_fence(&cargo_toml_path)? {
                        println!("✅ Added local [workspace] fence for standalone Rust function");
                    }
                    println!(
                        "ℹ️  Using standalone Rust workspace outside any managed functions root."
                    );
                }

                println!(
                    "\n🎉 Function initialized successfully at {}",
                    function_dir.display()
                );
                println!("Next steps:");
                println!("  1. Implement actions in src/lib.rs");
                println!("  2. Refine request/response schemas in openapi.yaml");
                println!("  3. Add shared DTOs or support packages if needed");
                println!(
                    "  4. Run: cd {} && cargo check --target wasm32-unknown-unknown",
                    function_dir.display()
                );
                println!(
                    "  5. Run: cd {} && picusbase deploy functions --rebuild",
                    function_dir.display()
                );

                Ok(())
            }
        },
        Commands::Bundle(args) => match args.command {
            BundleCommands::Js { src, out } => {
                run_bundle(bundler::BundleKind::JsBundleV1, &src, &out, None)
            }
            BundleCommands::Python {
                src,
                out,
                runtime_class,
            } => run_bundle(
                bundler::BundleKind::PySourceBundleV1,
                &src,
                &out,
                Some(runtime_class.as_str()),
            ),
        },
        Commands::Dev(args) => {
            let cwd = std::env::current_dir()?;
            let platform_dir = find_platform_dir(&cwd)?;
            let scripts_dir = platform_dir.join("infrastructure").join("scripts");

            match args.command {
                DevCommands::Setup {
                    reset,
                    no_build,
                    no_load,
                    cluster,
                    with_apps,
                    app_tag,
                } => {
                    let script = scripts_dir.join("setup-local.sh");
                    let mut envs: Vec<(&str, String)> = Vec::new();
                    if reset {
                        envs.push(("RESET_CLUSTER", "1".to_string()));
                    }
                    if no_build {
                        envs.push(("BUILD_IMAGES", "0".to_string()));
                    }
                    if no_load {
                        envs.push(("LOAD_IMAGES", "0".to_string()));
                    }
                    if let Some(name) = cluster {
                        envs.push(("CLUSTER_NAME", name));
                    }
                    let status = std::process::Command::new("bash")
                        .arg(&script)
                        .envs(envs)
                        .current_dir(&platform_dir)
                        .status()
                        .with_context(|| format!("Failed to run {}", script.display()))?;
                    if !status.success() {
                        anyhow::bail!("setup-local.sh exited with status {}", status);
                    }

                    if with_apps {
                        // Discover app.toml files in the repo and deploy each
                        use app_commands::EnvConfig;
                        use app_config::AppConfig;
                        let repo_root = platform_dir
                            .parent()
                            .map(|p| p.to_path_buf())
                            .unwrap_or_else(|| cwd.clone());
                        let env_cfg = EnvConfig::from_env_name("local", &platform_dir)?;
                        let mut app_tomls: Vec<std::path::PathBuf> = Vec::new();
                        for entry in walkdir::WalkDir::new(&repo_root)
                            .max_depth(5)
                            .into_iter()
                            .filter_map(|e| e.ok())
                        {
                            if entry.file_name() == "app.toml" {
                                app_tomls.push(entry.path().to_path_buf());
                            }
                        }
                        if app_tomls.is_empty() {
                            println!("--with-apps: no app.toml files found under {}", repo_root.display());
                        } else {
                            for app_toml in &app_tomls {
                                println!("--with-apps: deploying {}", app_toml.display());
                                let app_config = match AppConfig::load(app_toml) {
                                    Ok(c) => c,
                                    Err(e) => {
                                        eprintln!("  Warning: failed to parse {}: {}", app_toml.display(), e);
                                        continue;
                                    }
                                };
                                app_commands::register_app_in_values(&env_cfg.helm_values_file, &app_config)
                                    .unwrap_or_else(|e| eprintln!("  Warning: register: {}", e));
                                match app_commands::run_build(&app_config, app_toml, &app_tag, &env_cfg, &[], false) {
                                    Ok(_) => {}
                                    Err(e) => { eprintln!("  Warning: build failed: {}", e); continue; }
                                }
                                app_commands::patch_helm_values(&env_cfg.helm_values_file, &app_config, &app_tag)
                                    .unwrap_or_else(|e| eprintln!("  Warning: patch: {}", e));
                            }
                            app_commands::copy_registry_secret(&env_cfg)
                                .unwrap_or_else(|e| eprintln!("  Warning: {}", e));
                            app_commands::run_helm_deploy(&env_cfg)
                                .unwrap_or_else(|e| eprintln!("  Warning: helm deploy: {}", e));
                        }
                    }
                    Ok(())
                }
                DevCommands::Reload { service, cluster } => {
                    let script = scripts_dir.join("setup-local.sh");
                    let mut envs: Vec<(&str, String)> = Vec::new();
                    if let Some(name) = cluster {
                        envs.push(("CLUSTER_NAME", name));
                    }
                    let status = std::process::Command::new("bash")
                        .arg(&script)
                        .arg("reload")
                        .arg(&service)
                        .envs(envs)
                        .current_dir(&platform_dir)
                        .status()
                        .with_context(|| format!("Failed to run {}", script.display()))?;
                    if !status.success() {
                        anyhow::bail!("setup-local.sh reload exited with status {}", status);
                    }
                    Ok(())
                }
                DevCommands::Build {
                    service,
                    tag,
                    platform,
                } => {
                    let script = scripts_dir.join("build-images.sh");
                    let mut args: Vec<String> = vec!["--load".to_string()];
                    for s in &service {
                        args.push("--service".to_string());
                        args.push(s.clone());
                    }
                    if let Some(t) = tag {
                        args.push("--tag".to_string());
                        args.push(t);
                    }
                    if let Some(p) = platform {
                        args.push("--platform".to_string());
                        args.push(p);
                    }
                    let status = std::process::Command::new("bash")
                        .arg(&script)
                        .args(&args)
                        .current_dir(&platform_dir)
                        .status()
                        .with_context(|| format!("Failed to run {}", script.display()))?;
                    if !status.success() {
                        anyhow::bail!("build-images.sh exited with status {}", status);
                    }
                    Ok(())
                }
                DevCommands::Push { service, tag } => {
                    let script = scripts_dir.join("push-images.sh");
                    let mut args: Vec<String> = Vec::new();
                    for s in &service {
                        args.push("--service".to_string());
                        args.push(s.clone());
                    }
                    if let Some(t) = tag {
                        args.push("--tag".to_string());
                        args.push(t);
                    }
                    let status = std::process::Command::new("bash")
                        .arg(&script)
                        .args(&args)
                        .current_dir(&platform_dir)
                        .status()
                        .with_context(|| format!("Failed to run {}", script.display()))?;
                    if !status.success() {
                        anyhow::bail!("push-images.sh exited with status {}", status);
                    }
                    Ok(())
                }
                DevCommands::ApplySecret {
                    env_file,
                    namespace,
                } => {
                    let script = scripts_dir.join("apply-runtime-secret.sh");
                    let mut args: Vec<String> = Vec::new();
                    if let Some(path) = env_file {
                        args.push("--env-file".to_string());
                        args.push(path.to_string_lossy().into_owned());
                    }
                    if let Some(ns) = namespace {
                        args.push("--namespace".to_string());
                        args.push(ns);
                    }
                    let status = std::process::Command::new("bash")
                        .arg(&script)
                        .args(&args)
                        .current_dir(&platform_dir)
                        .status()
                        .with_context(|| format!("Failed to run {}", script.display()))?;
                    if !status.success() {
                        anyhow::bail!("apply-runtime-secret.sh exited with status {}", status);
                    }
                    Ok(())
                }
                DevCommands::PortForward => {
                    let script = scripts_dir.join("port-forward-local-kind-functions.sh");
                    let status = std::process::Command::new("bash")
                        .arg(&script)
                        .current_dir(&platform_dir)
                        .status()
                        .with_context(|| format!("Failed to run {}", script.display()))?;
                    if !status.success() {
                        anyhow::bail!(
                            "port-forward-local-kind-functions.sh exited with status {}",
                            status
                        );
                    }
                    Ok(())
                }
            }
        }
        Commands::Cluster(args) => {
            let cwd = std::env::current_dir()?;
            let platform_dir = find_platform_dir(&cwd)?;
            let scripts_dir = platform_dir.join("infrastructure").join("scripts");
            // Standalone infra chart — see PLAN-cluster-manager-deploy.md Phase M. Its own
            // values.yaml already carries local-dev-ready defaults (runtimeSecret.create: true),
            // unlike the app chart above which needs an environment values file passed in.
            let kube_manager_chart_dir = platform_dir
                .join("infrastructure")
                .join("helm")
                .join("kube-manager");

            match args.command {
                ClusterCommands::PrepareLocal { cluster } => {
                    let script = scripts_dir.join("setup-local.sh");
                    let mut envs: Vec<(&str, String)> = Vec::new();
                    if let Some(name) = cluster {
                        envs.push(("CLUSTER_NAME", name));
                    }
                    let status = std::process::Command::new("bash")
                        .arg(&script)
                        .arg("prepare-cluster")
                        .envs(envs)
                        .current_dir(&platform_dir)
                        .status()
                        .with_context(|| format!("Failed to run {}", script.display()))?;
                    if !status.success() {
                        anyhow::bail!("setup-local.sh prepare-cluster exited with status {}", status);
                    }
                    println!("Kubernetes (kind) is ready. Next: `picusbase cluster bootstrap`");
                    Ok(())
                }
                ClusterCommands::LoadImagesLocal { cluster } => {
                    let script = scripts_dir.join("setup-local.sh");
                    let mut envs: Vec<(&str, String)> = Vec::new();
                    if let Some(name) = cluster {
                        envs.push(("CLUSTER_NAME", name));
                    }
                    let status = std::process::Command::new("bash")
                        .arg(&script)
                        .arg("load-images")
                        .envs(envs)
                        .current_dir(&platform_dir)
                        .status()
                        .with_context(|| format!("Failed to run {}", script.display()))?;
                    if !status.success() {
                        anyhow::bail!("setup-local.sh load-images exited with status {}", status);
                    }
                    Ok(())
                }
                ClusterCommands::Bootstrap {
                    namespace,
                    secret_name,
                    release,
                    extra_values,
                } => {
                    // Deploys the STANDALONE infra chart (infrastructure/helm/kube-manager,
                    // see PLAN-cluster-manager-deploy.md Phase M) — a separate, permanent
                    // release from any picusbase app instance. Its own values.yaml already
                    // has local-dev-ready defaults (runtimeSecret.create: true with real
                    // credentials baked in), so --extra-values is only needed to override
                    // those for a non-local environment. Without it (product), pass
                    // --extra-values pointing at a values file with `runtimeSecret.create:
                    // false`, and create the bootstrap token/admin credentials out-of-band
                    // first — see generate-kube-manager-bootstrap-token.sh.
                    if let Some(extra) = &extra_values {
                        let extra_path = Path::new(extra);
                        let needs_out_of_band_secret = std::fs::read_to_string(extra_path)
                            .map(|contents| contents.contains("runtimeSecret") && contents.contains("create: false"))
                            .unwrap_or(false);
                        if needs_out_of_band_secret {
                            let token_script = scripts_dir.join("generate-kube-manager-bootstrap-token.sh");
                            let status = std::process::Command::new("bash")
                                .arg(&token_script)
                                .arg(&namespace)
                                .arg(secret_name.as_deref().unwrap_or("picusbase-bootstrap"))
                                .current_dir(&platform_dir)
                                .status()
                                .with_context(|| format!("Failed to run {}", token_script.display()))?;
                            if !status.success() {
                                anyhow::bail!(
                                    "generate-kube-manager-bootstrap-token.sh exited with status {}",
                                    status
                                );
                            }
                        }
                    }

                    let mut helm_args: Vec<String> = vec![
                        "upgrade".to_string(),
                        "--install".to_string(),
                        release.clone(),
                        kube_manager_chart_dir.to_str().unwrap_or_default().to_string(),
                        "--namespace".to_string(),
                        namespace.clone(),
                        "--create-namespace".to_string(),
                    ];
                    if let Some(extra) = &extra_values {
                        let extra_path = Path::new(extra);
                        let resolved = if extra_path.is_absolute() {
                            extra_path.to_path_buf()
                        } else {
                            platform_dir.join(extra_path)
                        };
                        helm_args.push("--values".to_string());
                        helm_args.push(resolved.to_string_lossy().into_owned());
                    }
                    helm_args.push("--wait".to_string());

                    let status = std::process::Command::new("helm")
                        .args(&helm_args)
                        .current_dir(&platform_dir)
                        .status()
                        .context("Failed to run helm upgrade --install for kube-manager bootstrap")?;
                    if !status.success() {
                        anyhow::bail!("helm upgrade --install (bootstrap) exited with status {}", status);
                    }

                    println!("kube-manager infra release {release} is up in namespace {namespace}.");
                    println!("This release stays separate from any app instance — install one with:");
                    println!("Next: `picusbase cluster install --values-file <...> --namespace <ns> --release <app-release> --token <bootstrap-token>`");
                    Ok(())
                }
                ClusterCommands::Install {
                    values_file,
                    namespace,
                    release,
                    kube_manager_url,
                    token,
                    skip_data_plane,
                } => run_cluster_apply(
                    "install",
                    &kube_manager_url,
                    &token,
                    &values_file,
                    &namespace,
                    &release,
                    skip_data_plane,
                ),
                ClusterCommands::Upgrade {
                    values_file,
                    namespace,
                    release,
                    kube_manager_url,
                    token,
                } => run_cluster_apply(
                    "upgrade",
                    &kube_manager_url,
                    &token,
                    &values_file,
                    &namespace,
                    &release,
                    true,
                ),
            }
        }
        Commands::App(args) => {
            use app_commands::EnvConfig;
            use app_config::AppConfig;

            let cwd = std::env::current_dir()?;
            let platform_dir = find_platform_dir(&cwd)?;

            // Resolve the picusbase CLI binary path (self)
            let picusbase_cli = std::env::current_exe()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| "picusbase".to_string());

            match args.command {
                AppCommands::Remove { config, env } => {
                    let config_path = resolve_absolute_path(config);
                    let app_config = AppConfig::load(&config_path)?;
                    let env_cfg = EnvConfig::from_env_name(&env, &platform_dir)?;

                    app_commands::remove_app_from_values(&env_cfg.helm_values_file, &app_config)?;
                    app_commands::run_helm_deploy(&env_cfg)?;
                    Ok(())
                }

                AppCommands::Build { config, tag, env, build_args, force_clean } => {
                    let config_path = resolve_absolute_path(config);
                    let app_config = AppConfig::load(&config_path)?;
                    let env_cfg = EnvConfig::from_env_name(&env, &platform_dir)?;
                    let extra: Vec<(String, String)> = build_args
                        .iter()
                        .filter_map(|s| {
                            let mut parts = s.splitn(2, '=');
                            Some((parts.next()?.to_string(), parts.next()?.to_string()))
                        })
                        .collect();
                    app_commands::run_build(&app_config, &config_path, &tag, &env_cfg, &extra, force_clean)?;
                    Ok(())
                }

                AppCommands::Push { config, tag, env } => {
                    let config_path = resolve_absolute_path(config);
                    let app_config = AppConfig::load(&config_path)?;
                    let env_cfg = EnvConfig::from_env_name(&env, &platform_dir)?;
                    app_commands::run_push(&app_config, &tag, &env_cfg)?;
                    Ok(())
                }

                AppCommands::Deploy { config, tag, env, project_ref, migrate, build_args } => {
                    let config_path = resolve_absolute_path(config);
                    let app_config = AppConfig::load(&config_path)?;
                    let env_cfg = EnvConfig::from_env_name(&env, &platform_dir)?;

                    // Run migrations if requested
                    if migrate {
                        let pref = project_ref.as_deref().or(resolved_project_ref.as_deref()).ok_or_else(|| {
                            anyhow::anyhow!("--project-ref is required when --migrate is used")
                        })?;
                        app_commands::run_migrations(&app_config, &config_path, pref, &env_cfg, &[])?;
                    }

                    // Ensure namespace exists
                    let _ = env_cfg.kubectl_base()
                        .args(["create", "namespace", &env_cfg.namespace, "--dry-run=client", "-o", "yaml"])
                        .output();

                    app_commands::copy_registry_secret(&env_cfg)
                        .unwrap_or_else(|e| eprintln!("Warning: {}", e));
                    app_commands::copy_internal_token_secret(&app_config, &env_cfg)
                        .unwrap_or_else(|e| eprintln!("Warning: {}", e));

                    let extra: Vec<(String, String)> = build_args
                        .iter()
                        .filter_map(|s| {
                            let mut parts = s.splitn(2, '=');
                            Some((parts.next()?.to_string(), parts.next()?.to_string()))
                        })
                        .collect();

                    // Build and push for product; just build+load for local
                    app_commands::run_build(&app_config, &config_path, &tag, &env_cfg, &extra, false)?;
                    if !env_cfg.is_local {
                        app_commands::run_push(&app_config, &tag, &env_cfg)?;
                    }

                    app_commands::register_app_in_values(&env_cfg.helm_values_file, &app_config)?;
                    app_commands::patch_helm_values(&env_cfg.helm_values_file, &app_config, &tag)?;
                    app_commands::run_helm_deploy(&env_cfg)?;
                    Ok(())
                }

                AppCommands::Migrate { config, project_ref, env, only } => {
                    let config_path = resolve_absolute_path(config);
                    let app_config = AppConfig::load(&config_path)?;
                    let env_cfg = EnvConfig::from_env_name(&env, &platform_dir)?;
                    let only_files: Vec<String> = only
                        .map(|s| s.split(',').map(|f| f.trim().to_string()).collect())
                        .unwrap_or_default();
                    app_commands::run_migrations(&app_config, &config_path, &project_ref, &env_cfg, &only_files)?;
                    Ok(())
                }

                AppCommands::Status { app_id, env } => {
                    let env_cfg = EnvConfig::from_env_name(&env, &platform_dir)?;
                    app_commands::run_app_status(&app_id, &env_cfg)?;
                    Ok(())
                }

                AppCommands::List { env } => {
                    let env_cfg = EnvConfig::from_env_name(&env, &platform_dir)?;
                    app_commands::run_app_list(&env_cfg)?;
                    Ok(())
                }

                AppCommands::Rollback { env, revision } => {
                    let env_cfg = EnvConfig::from_env_name(&env, &platform_dir)?;
                    app_commands::run_helm_rollback(&env_cfg, revision)?;
                    Ok(())
                }

                AppCommands::Start {
                    config, project_ref, tag, env,
                    skip_migrate, skip_functions, skip_sync, skip_build,
                    only_functions, build_args, force_clean,
                } => {
                    let config_path = resolve_absolute_path(config);
                    let app_config = AppConfig::load(&config_path)?;
                    let env_cfg = EnvConfig::from_env_name(&env, &platform_dir)?;
                    let extra: Vec<(String, String)> = build_args
                        .iter()
                        .filter_map(|s| {
                            let mut parts = s.splitn(2, '=');
                            Some((parts.next()?.to_string(), parts.next()?.to_string()))
                        })
                        .collect();
                    let opts = app_commands::StartOptions {
                        skip_migrate,
                        skip_functions,
                        skip_sync,
                        skip_build,
                        only_functions,
                        extra_build_args: extra,
                        force_clean,
                    };
                    app_commands::run_start(
                        &app_config, &config_path, &project_ref, &tag,
                        &env_cfg, &opts, &picusbase_cli,
                    )?;
                    Ok(())
                }

                AppCommands::Functions(fargs) => {
                    match fargs.command {
                        AppFunctionsCommands::Deploy { config, project_ref, env, function } => {
                            let config_path = resolve_absolute_path(config);
                            let app_config = AppConfig::load(&config_path)?;
                            let env_cfg = EnvConfig::from_env_name(&env, &platform_dir)?;
                            app_commands::run_functions_deploy(
                                &app_config, &config_path, &project_ref,
                                &env_cfg, function.as_deref(), &picusbase_cli,
                            )?;
                            Ok(())
                        }
                        AppFunctionsCommands::Sync { config, project_ref, env } => {
                            let config_path = resolve_absolute_path(config);
                            let _app_config = AppConfig::load(&config_path)?;
                            let env_cfg = EnvConfig::from_env_name(&env, &platform_dir)?;
                            // Delegate to existing sync shell script in picus-erp
                            let app_dir = AppConfig::app_dir(&config_path);
                            let sync_script = app_dir
                                .ancestors()
                                .find(|p| p.join("infrastructure").join("helm").join("sync-business-functions.sh").exists())
                                .map(|p| p.join("infrastructure").join("helm").join("sync-business-functions.sh"));
                            if let Some(script) = sync_script {
                                let mut cmd = std::process::Command::new("bash");
                                cmd.arg(&script).arg(&project_ref);
                                if let Some(ref kc) = env_cfg.kubeconfig {
                                    cmd.env("KUBECONFIG", kc);
                                }
                                let status = cmd.status().context("Failed to run sync script")?;
                                if !status.success() {
                                    anyhow::bail!("sync-business-functions.sh failed");
                                }
                            } else {
                                eprintln!("Warning: sync-business-functions.sh not found. Sync skipped.");
                            }
                            Ok(())
                        }
                    }
                }
            }
        }

        Commands::Platform(args) => {
            match args.command {
                PlatformCommands::OrgCreate { name } => {
                    let body = serde_json::json!({ "display_name": name });
                    let (status, text) = client.send_api("POST", "/platform/organizations", Some(&body))?;
                    if !(200..300).contains(&status) {
                        anyhow::bail!("org create failed (HTTP {status}): {text}");
                    }
                    let json: serde_json::Value = serde_json::from_str(&text)?;
                    println!("Organization created.");
                    println!("  organization_id : {}", json["id"].as_str().unwrap_or("<unknown>"));
                    println!("  name            : {}", name);
                    Ok(())
                }

                PlatformCommands::ProjectCreate { org_id, name } => {
                    let body = serde_json::json!({ "name": name, "organization_id": org_id });
                    let (status, text) = client.send_api("POST", "/platform/projects", Some(&body))?;
                    if !(200..300).contains(&status) {
                        anyhow::bail!("project create failed (HTTP {status}): {text}");
                    }
                    let json: serde_json::Value = serde_json::from_str(&text)?;
                    let project_ref = json["project_ref"].as_str().unwrap_or("<unknown>");
                    println!("Project created. Provisioning may take a few seconds.");
                    println!("  project_ref     : {}", project_ref);
                    println!("  name            : {}", name);
                    println!("  organization_id : {}", org_id);
                    println!();
                    println!("Next: wait ~5s, then pass --project-ref {} to later commands.", project_ref);
                    Ok(())
                }

                PlatformCommands::OrgList { console_url } => {
                    let tok = client.bearer_token()?;
                    let base = console_url.as_deref().unwrap_or_else(|| client.base_url());
                    let resp = reqwest::blocking::Client::new()
                        .get(format!("{}/platform/organizations", base))
                        .header("authorization", format!("Bearer {}", tok))
                        .send()
                        .context("Failed to list organizations")?;
                    let text = resp.text().unwrap_or_default();
                    let json: serde_json::Value = serde_json::from_str(&text)
                        .unwrap_or(serde_json::json!([]));
                    let orgs = json.as_array().cloned().unwrap_or_default();
                    if orgs.is_empty() {
                        println!("No organizations found.");
                    } else {
                        println!("{:<38}  {}", "ID", "Name");
                        println!("{}", "-".repeat(60));
                        for org in &orgs {
                            let id = org.get("id").and_then(|v| v.as_str()).unwrap_or("-");
                            let name = org.get("display_name").and_then(|v| v.as_str()).unwrap_or("-");
                            println!("{:<38}  {}", id, name);
                        }
                    }
                    Ok(())
                }

                PlatformCommands::ProjectList { org_id, console_url } => {
                    let tok = client.bearer_token()?;
                    let base = console_url.as_deref().unwrap_or_else(|| client.base_url());
                    let url = if let Some(ref oid) = org_id {
                        format!("{}/platform/projects?organization_id={}", base, oid)
                    } else {
                        format!("{}/platform/projects", base)
                    };
                    let resp = reqwest::blocking::Client::new()
                        .get(&url)
                        .header("authorization", format!("Bearer {}", tok))
                        .send()
                        .context("Failed to list projects")?;
                    let text = resp.text().unwrap_or_default();
                    let json: serde_json::Value = serde_json::from_str(&text)
                        .unwrap_or(serde_json::json!([]));
                    let projects = json.as_array().cloned().unwrap_or_default();
                    if projects.is_empty() {
                        println!("No projects found.");
                    } else {
                        println!("{:<15}  {:<38}  {}", "project_ref", "organization_id", "name");
                        println!("{}", "-".repeat(80));
                        for p in &projects {
                            let pref = p.get("project_ref").and_then(|v| v.as_str()).unwrap_or("-");
                            let oid = p.get("organization_id").and_then(|v| v.as_str()).unwrap_or("-");
                            let name = p.get("name").and_then(|v| v.as_str()).unwrap_or("-");
                            println!("{:<15}  {:<38}  {}", pref, oid, name);
                        }
                    }
                    Ok(())
                }

                PlatformCommands::ProjectStatus { project_ref } => {
                    let (status, text) = client.send_api("GET", &format!("/platform/projects/{project_ref}"), None)?;
                    if !(200..300).contains(&status) {
                        anyhow::bail!("project status failed (HTTP {status}): {text}");
                    }
                    let json: serde_json::Value = serde_json::from_str(&text)?;
                    println!("project_ref : {}", project_ref);
                    println!("status      : {}", json["status"].as_str().unwrap_or("<unknown>"));
                    Ok(())
                }
            }
        }

        Commands::Upgrade => run_upgrade(),
    }
}

fn run_bundle(
    kind: bundler::BundleKind,
    src: &Path,
    out: &Path,
    runtime_class: Option<&str>,
) -> Result<()> {
    let allowed_capabilities = PicusConfig::load_from_dir(src)
        .and_then(|config| config.allowed_capabilities())
        .with_context(|| {
            format!(
                "Bundle source {} must provide a valid picus.toml capability manifest",
                src.display()
            )
        })?;
    let runtime_class = runtime_class.unwrap_or(kind.runtime_class());
    println!(
        "📦 Packaging {} bundle from {} → {}",
        runtime_class,
        src.display(),
        out.display()
    );
    let artifact = bundler::package_bundle_with_capabilities_for_runtime_class(
        kind,
        src,
        out,
        &allowed_capabilities,
        runtime_class,
    )?;
    println!(
        "✅ Wrote {} bytes to {} ({} validated source file{})",
        artifact.archive_size,
        artifact.archive_path.display(),
        artifact.validated_files.len(),
        if artifact.validated_files.len() == 1 {
            ""
        } else {
            "s"
        }
    );
    println!(
        "   Next: picusbase function upload --id <key> --file {} --runtime-class {} --language {}",
        artifact.archive_path.display(),
        runtime_class,
        kind.language()
    );
    Ok(())
}
