//! `picusbase deploy schema` — deploy the data layer of a bundle directory into a project:
//! `doctypes/<code>/*.yaml` (entities), `roles/<code>/role.yaml`, `functions/<code>/`,
//! `automations/*.json`, `ai_agent/agents/*.yaml`. UI lives in separate frontends
//! (`picusbase deploy app`).
//!
//! Role → permission resolution runs server-side (`POST .../schema/roles`, same crate the
//! platform validates with); the merged `permissions.yaml` files come back and are written to disk
//! before each entity is defined, drafted and published.
use anyhow::{bail, Context, Result};
use reqwest::blocking::Client;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::client::PicusClient;

pub struct SchemaDeployOptions {
    pub skip_functions: bool,
    pub no_publish: bool,
    pub bound_user_id: Option<String>,
    /// Check and report only: nothing is written to the project.
    pub dry_run: bool,
    /// A failing function does not stop the others; every failure is reported at the end.
    pub keep_going: bool,
}

/// Roles the platform provides in every project; a permission naming one without a `role.yaml` is normal.
const BUILT_IN_ROLES: [&str; 4] = ["admin", "anon", "authenticated", "project-members"];

/// The server warns about every role named in a `permissions.yaml` that has no `roles/<x>/role.yaml`.
fn is_built_in_role_warning(warning: &str) -> bool {
    warning
        .strip_prefix("role '")
        .and_then(|rest| rest.split('\'').next())
        .is_some_and(|name| BUILT_IN_ROLES.contains(&name))
}

/// Names for a summary line: all of them up to `limit`, then how many were left out.
fn list_names(names: &[String], limit: usize) -> String {
    if names.len() <= limit {
        names.join(", ")
    } else {
        format!("{}, … and {} more", names[..limit].join(", "), names.len() - limit)
    }
}

struct Api<'a> {
    http: Client,
    base: String,
    project_ref: &'a str,
    token: String,
}

impl Api<'_> {
    fn url(&self, path: &str) -> String {
        format!("{}/platform/projects/{}{path}", self.base, self.project_ref)
    }

    fn send(&self, request: reqwest::blocking::RequestBuilder, what: &str) -> Result<Value> {
        let res = request.bearer_auth(&self.token).send().with_context(|| format!("{what}: request failed"))?;
        let status = res.status();
        let body = res.text().unwrap_or_default();
        if !status.is_success() {
            bail!("{what} failed ({status}): {body}");
        }
        Ok(serde_json::from_str(&body).unwrap_or(Value::Null))
    }

    fn post(&self, path: &str, body: &Value, what: &str) -> Result<Value> {
        self.send(self.http.post(self.url(path)).json(body), what)
    }

    fn get(&self, path: &str, what: &str) -> Result<Value> {
        self.send(self.http.get(self.url(path)), what)
    }
}

type Files = BTreeMap<String, String>;

/// `<parent>/<code>/*.yaml` → code → (filename → content).
fn scan_yaml_dirs(parent: &Path) -> Result<BTreeMap<String, Files>> {
    let mut out = BTreeMap::new();
    if !parent.is_dir() {
        return Ok(out);
    }
    for entry in std::fs::read_dir(parent).with_context(|| format!("Failed to read {}", parent.display()))? {
        let dir = entry?.path();
        if !dir.is_dir() {
            continue;
        }
        let code = dir.file_name().unwrap().to_string_lossy().to_string();
        let mut files = Files::new();
        for file in std::fs::read_dir(&dir)? {
            let path = file?.path();
            if path.extension().and_then(|e| e.to_str()) == Some("yaml") {
                files.insert(path.file_name().unwrap().to_string_lossy().to_string(), std::fs::read_to_string(&path)?);
            }
        }
        if !files.is_empty() {
            out.insert(code, files);
        }
    }
    Ok(out)
}

fn yaml_to_json(text: &str, what: &str) -> Result<Value> {
    let value: serde_yaml::Value = serde_yaml::from_str(text).with_context(|| format!("Invalid YAML in {what}"))?;
    Ok(serde_json::to_value(value)?)
}

/// Entities referenced by a `relation` field are defined first where there is an order to follow. Entities that
/// reference each other have none, and the server accepts a relation to an entity defined later, so they go in
/// name order.
fn deploy_order(doctypes: &BTreeMap<String, Value>) -> Vec<String> {
    let deps: BTreeMap<&str, BTreeSet<&str>> = doctypes
        .iter()
        .map(|(code, doc)| {
            let targets = doc["fields"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|f| f["data_type"] == "relation")
                .filter_map(|f| f["validation_rules"]["target_entity_code"].as_str())
                .filter(|target| *target != code && doctypes.contains_key(*target))
                .collect();
            (code.as_str(), targets)
        })
        .collect();
    let mut order: Vec<String> = Vec::new();
    let mut done: HashSet<&str> = HashSet::new();
    loop {
        let ready: Vec<&str> = deps
            .iter()
            .filter(|(code, targets)| !done.contains(*code) && targets.iter().all(|t| done.contains(t)))
            .map(|(code, _)| *code)
            .collect();
        if ready.is_empty() {
            break;
        }
        for code in ready {
            done.insert(code);
            order.push(code.to_string());
        }
    }
    let cyclic: Vec<String> = deps.keys().filter(|c| !done.contains(*c)).map(|c| c.to_string()).collect();
    if !cyclic.is_empty() {
        println!(
            "  (entities that reference each other: {} — deployed in name order; the server accepts relations to entities defined later)",
            cyclic.join(", ")
        );
        order.extend(cyclic);
    }
    order
}

fn jwt_subject(token: &str) -> Option<String> {
    use base64::Engine;
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    serde_json::from_slice::<Value>(&bytes).ok()?.get("sub")?.as_str().map(str::to_string)
}

pub fn run(
    client: &PicusClient,
    bundle_dir: &Path,
    project_ref: &str,
    personal_token: Option<String>,
    options: SchemaDeployOptions,
) -> Result<()> {
    if !bundle_dir.is_dir() {
        bail!("Bundle directory not found: {}", bundle_dir.display());
    }
    let api = Api { http: Client::new(), base: client.base_url().trim_end_matches('/').to_string(), project_ref, token: client.bearer_token()? };

    let doctypes_dir = bundle_dir.join("doctypes");
    let mut entities = scan_yaml_dirs(&doctypes_dir)?;
    let roles: BTreeMap<String, String> = scan_yaml_dirs(&bundle_dir.join("roles"))?
        .into_iter()
        .filter_map(|(code, mut files)| files.remove("role.yaml").map(|text| (code, text)))
        .collect();
    println!("Found {} entities, {} roles in {}", entities.len(), roles.len(), bundle_dir.display());

    // Everything a function build can be rejected for is checked before the first write, so a bad
    // function stops the deploy before entities and roles are published, not halfway through.
    let mut failed_functions: Vec<(PathBuf, String)> = Vec::new();
    let mut deployable: Vec<PathBuf> = Vec::new();
    let scan = scan_functions(&bundle_dir.join("functions"))?;
    if !options.skip_functions {
        deployable = scan.dirs.clone();
        let failures = preflight_functions(&scan.dirs);
        if !failures.is_empty() {
            // A dry run reports them in its own summary.
            if !options.dry_run {
                println!("\nFunctions that cannot be built:\n{}", describe_failures(&failures));
            }
            if !options.keep_going && !options.dry_run {
                bail!(
                    "{} function(s) cannot be built, so nothing was deployed. Fix them, or use --skip-functions, or --keep-going to deploy the rest.",
                    failures.len()
                );
            }
            deployable.retain(|dir| !failures.iter().any(|(failed, _)| failed == dir));
            failed_functions.extend(failures);
        }
    }

    if options.dry_run {
        return dry_run(&api, bundle_dir, &entities, roles.len(), &scan, &failed_functions, options.skip_functions);
    }

    if !entities.is_empty() {
        println!("\nRoles & permissions:");
        let result = api.post("/schema/roles", &json!({ "entities": entities, "roles": roles }), "resolve roles")?;
        for role in result["roles"].as_array().into_iter().flatten() {
            let action = role["action"].as_str().unwrap_or("");
            if action != "exists" && action != "system" {
                println!("  {action} role '{}'", role["role"].as_str().unwrap_or("?"));
            }
        }
        for warning in result["warnings"].as_array().into_iter().flatten() {
            let warning = warning.as_str().unwrap_or_default();
            if !is_built_in_role_warning(warning) {
                println!("  ⚠️  {warning}");
            }
        }
        let generated: HashMap<String, String> = serde_json::from_value(result["generated_permissions"].clone()).unwrap_or_default();
        for (code, text) in &generated {
            if entities.get(code).and_then(|f| f.get("permissions.yaml")) != Some(text) {
                std::fs::write(doctypes_dir.join(code).join("permissions.yaml"), text)?;
                println!("  ~ doctypes/{code}/permissions.yaml regenerated from roles/");
            }
            entities.entry(code.clone()).or_default().insert("permissions.yaml".to_string(), text.clone());
        }

        let doctypes: BTreeMap<String, Value> = entities
            .iter()
            .map(|(code, files)| {
                let text = files.get("doctype.yaml").with_context(|| format!("doctypes/{code}/doctype.yaml is missing"))?;
                Ok((code.clone(), yaml_to_json(text, &format!("doctypes/{code}/doctype.yaml"))?))
            })
            .collect::<Result<_>>()?;

        println!("\nEntities:");
        let mut invalid = Vec::new();
        for code in deploy_order(&doctypes) {
            api.post("/entities", &doctypes[&code], &format!("define entity '{code}'"))?;
            let draft = api.post(
                &format!("/builder/entity/{code}/versions"),
                &json!({ "files": entities[&code] }),
                &format!("draft entity '{code}'"),
            )?;
            let seq = draft["version_seq"].as_i64().unwrap_or(-1);
            if draft["validation_status"] != "valid" {
                println!("  ✗ {code} v{seq}: {}", draft["validation_errors"]);
                invalid.push(code);
                continue;
            }
            // Every deploy adds a version, so v1 is an entity the project did not have.
            let kind = if seq == 1 { "new" } else { "new version" };
            if options.no_publish {
                println!("  ✓ {code} v{seq} drafted ({kind})");
                continue;
            }
            let id = draft["id"].as_str().unwrap_or_default();
            api.post(&format!("/builder/entity/{code}/versions/{id}/publish"), &Value::Null, &format!("publish entity '{code}'"))?;
            println!("  ✓ {code} v{seq} ({kind})");
        }
        if !invalid.is_empty() {
            bail!("{} entity version(s) failed validation: {}", invalid.len(), invalid.join(", "));
        }
    }

    if options.skip_functions {
        println!("\nFunctions: skipped (--skip-functions)");
    } else {
        failed_functions.extend(deploy_functions(&scan, &deployable, client, project_ref, personal_token, options.keep_going)?);
    }
    restore_automations(&api, &bundle_dir.join("automations"))?;
    let bound_user = options.bound_user_id.or_else(|| jwt_subject(&api.token));
    restore_ai_agents(&api, &bundle_dir.join("ai_agent").join("agents"), bound_user.as_deref())?;

    if !failed_functions.is_empty() {
        bail!("Deployed, except {} function(s) that failed:\n{}", failed_functions.len(), describe_failures(&failed_functions));
    }
    println!("\nDone.");
    Ok(())
}

/// Failures grouped by reason, so 75 functions missing the same dependency read as one line.
fn describe_failures(failures: &[(PathBuf, String)]) -> String {
    let mut groups: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for (dir, reason) in failures {
        groups.entry(reason.as_str()).or_default().push(dir_name(dir));
    }
    groups
        .into_iter()
        .map(|(reason, names)| format!("  ✗ {} function(s): {reason}\n      {}", names.len(), list_names(&names, 6)))
        .collect::<Vec<_>>()
        .join("\n")
}

fn dir_name(dir: &Path) -> String {
    dir.file_name().map(|name| name.to_string_lossy().to_string()).unwrap_or_else(|| dir.display().to_string())
}

/// `--dry-run`: what a real run would create or update, from the bundle and read-only calls. Roles are
/// not resolved here: that call writes.
fn dry_run(
    api: &Api,
    bundle_dir: &Path,
    entities: &BTreeMap<String, Files>,
    role_files: usize,
    scan: &FunctionScan,
    failed_functions: &[(PathBuf, String)],
    skip_functions: bool,
) -> Result<()> {
    println!("\nDry run: nothing is written to project {}.", api.project_ref);

    if !entities.is_empty() {
        let doctypes: BTreeMap<String, Value> = entities
            .iter()
            .map(|(code, files)| {
                let text = files.get("doctype.yaml").with_context(|| format!("doctypes/{code}/doctype.yaml is missing"))?;
                Ok((code.clone(), yaml_to_json(text, &format!("doctypes/{code}/doctype.yaml"))?))
            })
            .collect::<Result<_>>()?;
        let existing: HashSet<String> = api
            .get("/entities", "list entities")?
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|row| row["entity_code"].as_str().map(str::to_string))
            .collect();
        let (present, new): (Vec<String>, Vec<String>) = deploy_order(&doctypes).into_iter().partition(|code| existing.contains(code));
        println!("\nEntities ({}): {} new, {} already in the project (each gets a new version)", doctypes.len(), new.len(), present.len());
        if !new.is_empty() {
            println!("  + {}", list_names(&new, 40));
        }
    }
    println!("\nRoles: {role_files} role.yaml. Roles named in permissions.yaml are created or updated by the real run.");

    if skip_functions {
        println!("\nFunctions: skipped (--skip-functions)");
    } else {
        let deployed: HashSet<String> = api
            .get("/compute/functions", "list functions")?
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|row| row["function_key"].as_str().map(str::to_string))
            .collect();
        let (present, new): (Vec<String>, Vec<String>) = scan
            .dirs
            .iter()
            .filter_map(|dir| crate::config::PicusConfig::load_from_dir(dir).ok().map(|config| config.function.id))
            .partition(|id| deployed.contains(id));
        let blocked = if failed_functions.is_empty() { String::new() } else { format!(", {} cannot be built", failed_functions.len()) };
        println!("\nFunctions ({}): {} new, {} already deployed (rebuilt and published again){blocked}", scan.dirs.len(), new.len(), present.len());
        if !new.is_empty() {
            println!("  + {}", list_names(&new, 40));
        }
        if !failed_functions.is_empty() {
            println!("{}", describe_failures(failed_functions));
        }
        print_skipped(scan);
    }

    let automations = json_files(&bundle_dir.join("automations"))?;
    if !automations.is_empty() {
        let existing = existing_ids(api, "workflow_definition");
        let ids: Vec<String> = automations
            .iter()
            .filter_map(|path| serde_json::from_str::<Value>(&std::fs::read_to_string(path).ok()?).ok()?["id"].as_str().map(str::to_string))
            .collect();
        let known = ids.iter().filter(|id| existing.contains(*id)).count();
        println!("\nAutomations ({}): {} new, {known} already in the project (updated)", automations.len(), automations.len() - known);
    }

    let agents = yaml_files(&bundle_dir.join("ai_agent").join("agents"))?;
    if !agents.is_empty() {
        let existing = existing_agent_handles(api);
        let handles: Vec<String> = agents
            .iter()
            .filter_map(|path| yaml_to_json(&std::fs::read_to_string(path).ok()?, "agent").ok()?["handle"].as_str().map(str::to_string))
            .collect();
        let new: Vec<String> = handles.iter().filter(|h| !existing.contains(*h)).cloned().collect();
        println!("\nAI agents ({}): {} new, {} already present (skipped)", handles.len(), new.len(), handles.len() - new.len());
        if !new.is_empty() {
            println!("  + {}", list_names(&new, 40));
        }
    }
    Ok(())
}

fn json_files(dir: &Path) -> Result<Vec<PathBuf>> {
    sorted_files(dir, "json")
}

fn yaml_files(dir: &Path) -> Result<Vec<PathBuf>> {
    sorted_files(dir, "yaml")
}

fn sorted_files(dir: &Path, extension: &str) -> Result<Vec<PathBuf>> {
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|ext| ext == extension))
        .collect();
    files.sort();
    Ok(files)
}

/// Record ids of an entity; none when the entity is not defined yet.
fn existing_ids(api: &Api, entity: &str) -> HashSet<String> {
    api.get(&format!("/entities/{entity}/records?limit=1000"), &format!("list {entity} records"))
        .ok()
        .and_then(|rows| rows.as_array().map(|rows| rows.iter().filter_map(|row| row["id"].as_str().map(str::to_string)).collect()))
        .unwrap_or_default()
}

fn existing_agent_handles(api: &Api) -> HashSet<String> {
    api.get("/entities/ai_agent/records?limit=1000", "list ai_agent records")
        .ok()
        .and_then(|rows| {
            rows.as_array().map(|rows| rows.iter().filter_map(|row| row["data"]["handle"].as_str().map(str::to_string)).collect())
        })
        .unwrap_or_default()
}

/// `picusbase data restore` — replace entity records from `<dir>/*.sql` dumps
/// (`DELETE FROM entity_*` + `INSERT INTO entity_*`). Entities must be deployed first.
pub fn restore_data(client: &PicusClient, dir: &Path, project_ref: &str) -> Result<()> {
    let mut files = BTreeMap::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("Failed to read {}", dir.display()))? {
        let path = entry?.path();
        if path.extension().is_some_and(|ext| ext == "sql") {
            files.insert(format!("data/{}", path.file_name().unwrap().to_string_lossy()), std::fs::read_to_string(&path)?);
        }
    }
    if files.is_empty() {
        bail!("No .sql files in {}", dir.display());
    }
    let api = Api { http: Client::new(), base: client.base_url().trim_end_matches('/').to_string(), project_ref, token: client.bearer_token()? };
    println!("Restoring {} file(s) into {project_ref}...", files.len());
    let result = api.post("/data/restore", &json!({ "files": files }), "restore data")?;
    println!("✅ {} statements", result["statements"].as_u64().unwrap_or(0));
    Ok(())
}

struct FunctionScan {
    /// Directories with a `picus.toml`, sorted.
    dirs: Vec<PathBuf>,
    /// Entries of `functions/` that are not functions, with why (e.g. `target (build output)`).
    skipped: Vec<String>,
}

fn scan_functions(dir: &Path) -> Result<FunctionScan> {
    let mut scan = FunctionScan { dirs: Vec::new(), skipped: Vec::new() };
    if !dir.is_dir() {
        return Ok(scan);
    }
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)?.filter_map(|e| e.ok().map(|e| e.path())).collect();
    entries.sort();
    for path in entries {
        let name = dir_name(&path);
        if name.starts_with('.') {
            continue;
        }
        if path.join("picus.toml").exists() {
            scan.dirs.push(path);
            continue;
        }
        let why = if !path.is_dir() {
            "not a directory"
        } else if name == "target" {
            "build output"
        } else {
            "no picus.toml"
        };
        scan.skipped.push(format!("{name} ({why})"));
    }
    Ok(scan)
}

fn print_skipped(scan: &FunctionScan) {
    if !scan.skipped.is_empty() {
        println!("  not deployed: {}", scan.skipped.join(", "));
    }
}

/// Checks that need no build: `picus.toml` and the language's own requirements, and Docker for Rust.
fn preflight_functions(dirs: &[PathBuf]) -> Vec<(PathBuf, String)> {
    use crate::language::{adapter_for, detect_language, Language};
    let mut failures = Vec::new();
    let mut needs_docker = false;
    for dir in dirs {
        let check = || -> Result<Language> {
            crate::config::PicusConfig::load_from_dir(dir)?;
            let lang = detect_language(dir)?;
            adapter_for(lang).preflight(dir)?;
            Ok(lang)
        };
        match check() {
            Ok(lang) => needs_docker |= lang == Language::Rust,
            Err(error) => failures.push((dir.clone(), format!("{error:#}"))),
        }
    }
    if needs_docker {
        if let Err(error) = crate::language::require_docker() {
            // Every Rust function fails for the same reason: report it once, against all of them.
            let rust: Vec<&PathBuf> = dirs
                .iter()
                .filter(|dir| !failures.iter().any(|(failed, _)| failed == *dir) && detect_language(dir).is_ok_and(|lang| lang == Language::Rust))
                .collect();
            failures.extend(rust.into_iter().map(|dir| (dir.clone(), format!("{error:#}"))));
        }
    }
    failures
}

/// Deploys each function in order. A failure stops the run unless `keep_going`, in which case it is
/// collected and returned.
fn deploy_functions(
    scan: &FunctionScan,
    dirs: &[PathBuf],
    client: &PicusClient,
    project_ref: &str,
    personal_token: Option<String>,
    keep_going: bool,
) -> Result<Vec<(PathBuf, String)>> {
    let mut failed = Vec::new();
    if scan.dirs.is_empty() {
        print_skipped(scan);
        return Ok(failed);
    }
    println!("\nFunctions ({}):", scan.dirs.len());
    print_skipped(scan);
    for function_dir in dirs {
        let result = crate::deploy_function_dir(
            function_dir,
            client.base_url(),
            Some(project_ref.to_string()),
            None,
            None,
            None,
            None,
            personal_token.clone(),
        )
        .with_context(|| format!("Failed to deploy function {}", function_dir.display()));
        if let Err(error) = result {
            if !keep_going {
                eprintln!(
                    "\nEntities and roles were already published. Fix the function and run again (deploy schema can be repeated), or use --keep-going to deploy the other functions."
                );
                return Err(error);
            }
            eprintln!("✗ {:#}", error);
            failed.push((function_dir.clone(), format!("{error:#}")));
        }
    }
    Ok(failed)
}

/// `automations/<id>.json` → `workflow_definition` records, upserted by `id`.
fn restore_automations(api: &Api, dir: &Path) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
        .collect();
    files.sort();
    if files.is_empty() {
        return Ok(());
    }
    println!("\nAutomations:");
    for path in files {
        let row: Value = serde_json::from_str(&std::fs::read_to_string(&path)?).with_context(|| format!("Invalid JSON in {}", path.display()))?;
        let id = row["id"].as_str().unwrap_or_default().to_string();
        let name = row["workflow_name"].as_str().unwrap_or("(unnamed)").to_string();
        let body = json!({ "data": row });
        if !id.is_empty() {
            let res = api
                .http
                .patch(api.url(&format!("/entities/workflow_definition/records/{id}")))
                .bearer_auth(&api.token)
                .json(&body)
                .send()?;
            if res.status().is_success() {
                println!("  = {name}{}", id_suffix(&id));
                continue;
            }
            if res.status() != reqwest::StatusCode::NOT_FOUND {
                bail!("update automation '{name}' failed ({}): {}", res.status(), res.text().unwrap_or_default());
            }
        }
        api.post("/entities/workflow_definition/records", &body, &format!("create automation '{name}'"))?;
        println!("  + {name}{}", id_suffix(&id));
    }
    Ok(())
}

fn id_suffix(id: &str) -> String {
    if id.is_empty() {
        String::new()
    } else {
        format!(" ({id})")
    }
}

/// `ai_agent/agents/<handle>.yaml` → `ai_agent` records (created via the API so the
/// `after_create` hook provisions the agent principal). Skips handles that already exist;
/// `parent_handle` resolves to `parent_agent`, parents first.
fn restore_ai_agents(api: &Api, dir: &Path, bound_user_id: Option<&str>) -> Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|ext| ext == "yaml"))
        .collect();
    paths.sort();
    let mut pending: Vec<(String, Value)> = Vec::new();
    for path in &paths {
        let value = yaml_to_json(&std::fs::read_to_string(path)?, &path.display().to_string())?;
        let handle = value["handle"].as_str().with_context(|| format!("{}: missing 'handle'", path.display()))?.to_string();
        pending.push((handle, value));
    }
    if pending.is_empty() {
        return Ok(());
    }
    println!("\nAI agents:");

    let existing = api.send(api.http.get(api.url("/entities/ai_agent/records?limit=1000")), "list ai_agent records")?;
    let mut resolved: HashMap<String, String> = existing
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|row| Some((row["data"]["handle"].as_str()?.to_string(), row["id"].as_str()?.to_string())))
        .collect();
    pending.retain(|(handle, _)| !resolved.contains_key(handle));
    if pending.is_empty() {
        println!("  (all present)");
        return Ok(());
    }
    let bound_user_id = bound_user_id.context("--bound-user-id is required to create AI agents")?;

    while !pending.is_empty() {
        let before = pending.len();
        let mut waiting = Vec::new();
        for (handle, mut value) in pending {
            let parent = value["parent_handle"].as_str().map(str::to_string);
            if parent.as_ref().is_some_and(|p| !resolved.contains_key(p)) {
                waiting.push((handle, value));
                continue;
            }
            let obj = value.as_object_mut().with_context(|| format!("ai_agent '{handle}' is not a YAML object"))?;
            obj.remove("parent_handle");
            if let Some(parent) = parent {
                obj.insert("parent_agent".to_string(), json!(resolved[&parent]));
            }
            obj.insert("bound_user_id".to_string(), json!(bound_user_id));
            let created = api.post("/entities/ai_agent/records", &json!({ "data": value }), &format!("create ai_agent '{handle}'"))?;
            println!("  + {handle}");
            resolved.insert(handle, created["id"].as_str().unwrap_or_default().to_string());
        }
        pending = waiting;
        if pending.len() == before {
            let handles: Vec<&str> = pending.iter().map(|(h, _)| h.as_str()).collect();
            bail!("Unresolvable parent_handle for: {}", handles.join(", "));
        }
    }
    Ok(())
}
