use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

use crate::app_config::AppConfig;

// ── Env resolution ────────────────────────────────────────────────────────────

pub struct EnvConfig {
    pub kubeconfig: Option<String>,
    pub namespace: String,
    pub picusbase_namespace: String,
    pub pgdog_pf_port: u16,
    pub compute_cp_port: u16,
    pub registry: String,
    pub kind_cluster: String,
    pub is_local: bool,
    pub helm_chart_dir: PathBuf,
    pub helm_values_file: PathBuf,
    pub helm_release: String,
}

impl EnvConfig {
    pub fn from_env_name(env_name: &str, platform_dir: &Path) -> Result<Self> {
        let chart_dir = platform_dir
            .join("infrastructure")
            .join("helm")
            .join("picusbase-apps");

        match env_name {
            "local" | "local-kind" => Ok(EnvConfig {
                kubeconfig: None,
                namespace: "picusbase-apps".to_string(),
                picusbase_namespace: "picusbase".to_string(),
                pgdog_pf_port: 6432,
                compute_cp_port: 14080,
                registry: "registry.localhost:5001/picusbase-apps".to_string(),
                kind_cluster: "picusbase-local".to_string(),
                is_local: true,
                helm_values_file: chart_dir.join("values-local.yaml"),
                helm_chart_dir: chart_dir,
                helm_release: "picusbase-apps".to_string(),
            }),
            "product" | "prod" => {
                let kubeconfig = std::env::var("KUBECONFIG")
                    .ok()
                    .or_else(|| {
                        let default = format!(
                            "{}/.kube/picus-instance-b.yaml",
                            std::env::var("HOME").unwrap_or_default()
                        );
                        if Path::new(&default).exists() {
                            Some(default)
                        } else {
                            None
                        }
                    });
                Ok(EnvConfig {
                    kubeconfig,
                    namespace: "picusbase-apps".to_string(),
                    picusbase_namespace: "picusbase".to_string(),
                    pgdog_pf_port: 26432,
                    compute_cp_port: 24080,
                    registry: "registry.picusbase.com/picusbase-apps".to_string(),
                    kind_cluster: String::new(),
                    is_local: false,
                    helm_values_file: chart_dir.join("values-prod.yaml"),
                    helm_chart_dir: chart_dir,
                    helm_release: "picusbase-apps".to_string(),
                })
            }
            other => anyhow::bail!(
                "Unknown env '{}'. Supported: local, product",
                other
            ),
        }
    }

    pub fn kubectl_base(&self) -> Command {
        let mut cmd = Command::new("kubectl");
        if let Some(ref kc) = self.kubeconfig {
            cmd.arg("--kubeconfig").arg(kc);
        }
        cmd
    }
}

// ── Build ─────────────────────────────────────────────────────────────────────

pub fn run_build(
    config: &AppConfig,
    config_path: &Path,
    tag: &str,
    env: &EnvConfig,
    extra_build_args: &[(String, String)],
    force_clean: bool,
) -> Result<String> {
    let app_dir = AppConfig::app_dir(config_path);
    let build = config.build.as_ref();

    let context_dir = build
        .map(|b| app_dir.join(&b.context_dir))
        .unwrap_or_else(|| app_dir.clone());

    let dockerfile = app_dir.join(&config.app.dockerfile);
    if !dockerfile.exists() {
        anyhow::bail!("Dockerfile not found: {}", dockerfile.display());
    }

    let image_ref = format!("{}/{}:{}", env.registry, config.app.id, tag);

    // Place cache at the root of the build context, not inside app_dir.
    // When context_dir != app_dir (e.g. context_dir = ".."), placing the cache
    // inside app_dir puts it inside the context and inflates the transferred
    // context size on every build. Anchoring to context_dir keeps it at the
    // level already covered by the .dockerignore entry ".buildx-cache".
    let mut cache_dir = context_dir.clone();
    cache_dir.push(".buildx-cache");
    cache_dir.push(&config.app.id);

    println!("Building {} image: {}", config.app.runtime, image_ref);
    println!("  context : {}", context_dir.display());
    println!("  dockerfile: {}", dockerfile.display());

    let mut cmd = Command::new("docker");
    cmd.arg("buildx").arg("build");

    // Build context for picusbase shared packages
    if let Some(pb_ctx) = config.resolve_picusbase_context(&app_dir) {
        println!("  picusbase context: {}", pb_ctx.display());
        cmd.arg("--build-context")
            .arg(format!("picusbase={}", pb_ctx.display()));
    }

    // Build args from app.toml [build.args]
    if let Some(b) = build {
        for (k, v) in &b.args {
            cmd.arg("--build-arg").arg(format!("{}={}", k, v));
        }
    }

    // Extra build args from CLI
    for (k, v) in extra_build_args {
        cmd.arg("--build-arg").arg(format!("{}={}", k, v));
    }

    if force_clean {
        cmd.arg("--build-arg").arg("FORCE_CLEAN_BUILD=1");
    }

    // BuildKit cache
    if cache_dir.exists() {
        cmd.arg("--cache-from")
            .arg(format!("type=local,src={}", cache_dir.display()));
    }
    cmd.arg("--cache-to")
        .arg(format!("type=local,dest={},mode=max", cache_dir.display()));

    cmd.arg("-f").arg(&dockerfile);
    cmd.arg("-t").arg(&image_ref);

    if env.is_local {
        // Load into Docker daemon so kind can pick it up
        cmd.arg("--load");
    } else {
        // For product, just build locally; push is a separate step
        cmd.arg("--load");
    }

    cmd.arg(&context_dir);

    let status = cmd
        .status()
        .context("Failed to run docker buildx build")?;

    if !status.success() {
        anyhow::bail!("docker buildx build failed");
    }

    if env.is_local {
        println!("Loading image into kind cluster: {}", env.kind_cluster);
        let status = Command::new("kind")
            .arg("load")
            .arg("docker-image")
            .arg(&image_ref)
            .arg("--name")
            .arg(&env.kind_cluster)
            .status()
            .context("Failed to run kind load")?;
        if !status.success() {
            anyhow::bail!("kind load failed");
        }
        println!("Image loaded into kind cluster.");
    }

    println!("Build complete: {}", image_ref);
    Ok(image_ref)
}

// ── Push ──────────────────────────────────────────────────────────────────────

pub fn run_push(config: &AppConfig, tag: &str, env: &EnvConfig) -> Result<()> {
    let image_ref = format!("{}/{}:{}", env.registry, config.app.id, tag);
    println!("Pushing: {}", image_ref);
    let status = Command::new("docker")
        .arg("push")
        .arg(&image_ref)
        .status()
        .context("Failed to run docker push")?;
    if !status.success() {
        anyhow::bail!("docker push failed for {}", image_ref);
    }
    println!("Push complete: {}", image_ref);
    Ok(())
}

// ── Secret copy ───────────────────────────────────────────────────────────────

pub fn copy_registry_secret(env: &EnvConfig) -> Result<()> {
    println!(
        "Copying registry pull secret from {} to {}...",
        env.picusbase_namespace, env.namespace
    );

    let output = env
        .kubectl_base()
        .args([
            "get",
            "secret",
            "registry-picusbase-com",
            "-n",
            &env.picusbase_namespace,
            "-o",
            "jsonpath={.data.\\.dockerconfigjson}",
        ])
        .output()
        .context("Failed to get registry secret")?;

    if !output.status.success() {
        eprintln!(
            "Warning: could not get registry-picusbase-com from {}; skipping.",
            env.picusbase_namespace
        );
        return Ok(());
    }

    let dockerconfig_b64 = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if dockerconfig_b64.is_empty() {
        eprintln!("Warning: registry secret data is empty; skipping copy.");
        return Ok(());
    }

    let manifest = format!(
        r#"apiVersion: v1
kind: Secret
metadata:
  name: registry-picusbase-com
  namespace: {ns}
type: kubernetes.io/dockerconfigjson
data:
  .dockerconfigjson: {data}
"#,
        ns = env.namespace,
        data = dockerconfig_b64,
    );

    let mut child = env
        .kubectl_base()
        .args(["apply", "-f", "-"])
        .stdin(Stdio::piped())
        .spawn()
        .context("Failed to apply registry secret")?;

    use std::io::Write;
    if let Some(stdin) = child.stdin.take() {
        let mut stdin = stdin;
        stdin
            .write_all(manifest.as_bytes())
            .context("Failed to write secret manifest")?;
    }
    let status = child.wait().context("kubectl apply registry secret failed")?;
    if !status.success() {
        anyhow::bail!("Failed to copy registry secret to {}", env.namespace);
    }

    println!("Registry secret copied.");
    Ok(())
}

pub fn copy_internal_token_secret(app_config: &AppConfig, env: &EnvConfig) -> Result<()> {
    let source_secret = if env.is_local {
        "picusbase-bootstrap"
    } else {
        "picusbase-prod"
    };
    let key = "auth-internal-service-token";

    let output = env
        .kubectl_base()
        .args([
            "get",
            "secret",
            source_secret,
            "-n",
            &env.picusbase_namespace,
            "-o",
            &format!("jsonpath={{.data.{}}}", key),
        ])
        .output()
        .context("Failed to get internal service token secret")?;

    if !output.status.success() || output.stdout.is_empty() {
        eprintln!(
            "Warning: could not get {} from {}; PICUSBASE_AUTH_INTERNAL_SERVICE_TOKEN will not be injected.",
            source_secret, env.picusbase_namespace
        );
        return Ok(());
    }

    let token_b64 = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let secret_name = format!("picus-app-{}-{}", app_config.app.id, if env.is_local { "local" } else { "prod" });

    println!(
        "Copying PICUSBASE_AUTH_INTERNAL_SERVICE_TOKEN -> {}/{}...",
        env.namespace, secret_name
    );

    let manifest = format!(
        r#"apiVersion: v1
kind: Secret
metadata:
  name: {name}
  namespace: {ns}
type: Opaque
data:
  PICUSBASE_AUTH_INTERNAL_SERVICE_TOKEN: {data}
"#,
        name = secret_name,
        ns = env.namespace,
        data = token_b64,
    );

    let mut child = env
        .kubectl_base()
        .args(["apply", "-f", "-"])
        .stdin(Stdio::piped())
        .spawn()
        .context("Failed to apply internal token secret")?;

    use std::io::Write;
    if let Some(stdin) = child.stdin.take() {
        let mut stdin = stdin;
        stdin
            .write_all(manifest.as_bytes())
            .context("Failed to write secret manifest")?;
    }
    let status = child.wait().context("kubectl apply token secret failed")?;
    if !status.success() {
        anyhow::bail!("Failed to copy internal token secret to {}", env.namespace);
    }

    println!("Internal token secret copied: {}", secret_name);
    Ok(())
}

// ── Helm values patch ─────────────────────────────────────────────────────────

pub fn patch_helm_values(
    values_file: &Path,
    app_config: &AppConfig,
    tag: &str,
) -> Result<()> {
    let content = std::fs::read_to_string(values_file)
        .with_context(|| format!("Failed to read {}", values_file.display()))?;

    let mut doc: serde_yaml::Value = serde_yaml::from_str(&content)
        .with_context(|| format!("Failed to parse {}", values_file.display()))?;

    let helm_key = app_config.helm_key();

    // Ensure apps.<key> exists
    let apps = doc
        .get_mut("apps")
        .and_then(|v| v.as_mapping_mut())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "values file {} has no 'apps:' map",
                values_file.display()
            )
        })?;

    let app_entry = apps
        .entry(serde_yaml::Value::String(helm_key.clone()))
        .or_insert_with(|| serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));

    if let Some(mapping) = app_entry.as_mapping_mut() {
        mapping.insert(
            serde_yaml::Value::String("enabled".to_string()),
            serde_yaml::Value::Bool(true),
        );

        let image_entry = mapping
            .entry(serde_yaml::Value::String("image".to_string()))
            .or_insert_with(|| serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));

        if let Some(img) = image_entry.as_mapping_mut() {
            img.insert(
                serde_yaml::Value::String("tag".to_string()),
                serde_yaml::Value::String(tag.to_string()),
            );
        }
    }

    let updated = serde_yaml::to_string(&doc)
        .context("Failed to serialize updated values")?;
    std::fs::write(values_file, updated)
        .with_context(|| format!("Failed to write {}", values_file.display()))?;

    println!("Patched {}: apps.{}.image.tag = {}", values_file.display(), helm_key, tag);
    Ok(())
}

/// Register an app entry in the values file for the first time.
pub fn register_app_in_values(
    values_file: &Path,
    app_config: &AppConfig,
) -> Result<()> {
    let content = if values_file.exists() {
        std::fs::read_to_string(values_file)
            .with_context(|| format!("Failed to read {}", values_file.display()))?
    } else {
        "apps: {}\n".to_string()
    };

    let mut doc: serde_yaml::Value = serde_yaml::from_str(&content)
        .unwrap_or_else(|_| serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));

    if doc.get("apps").is_none() {
        if let Some(map) = doc.as_mapping_mut() {
            map.insert(
                serde_yaml::Value::String("apps".to_string()),
                serde_yaml::Value::Mapping(serde_yaml::Mapping::new()),
            );
        }
    }

    let helm_key = app_config.helm_key();
    let apps = doc
        .get_mut("apps")
        .and_then(|v| v.as_mapping_mut())
        .ok_or_else(|| anyhow::anyhow!("Cannot write apps section to values file"))?;

    // Check if entry is already fully registered (has service.port — the required Helm field).
    // A partial entry (e.g. created by apply_values_patch with only env vars) is treated as
    // unregistered and will have structural fields merged in below.
    let already_complete = apps
        .get(&serde_yaml::Value::String(helm_key.clone()))
        .and_then(|v| v.get("service"))
        .and_then(|s| s.get("port"))
        .is_some();

    if already_complete {
        println!("App '{}' already registered in {}", helm_key, values_file.display());
        return Ok(());
    }

    // Get or create the entry, preserving any existing fields (e.g. env vars from values_patch).
    let app_entry = apps
        .entry(serde_yaml::Value::String(helm_key.clone()))
        .or_insert_with(|| serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));

    let mapping = app_entry
        .as_mapping_mut()
        .ok_or_else(|| anyhow::anyhow!("App entry for '{}' is not a mapping", helm_key))?;

    let key = |s: &str| serde_yaml::Value::String(s.to_string());

    if !mapping.contains_key(&key("enabled")) {
        mapping.insert(key("enabled"), serde_yaml::Value::Bool(false));
    }
    if !mapping.contains_key(&key("name")) {
        mapping.insert(key("name"), serde_yaml::Value::String(app_config.app.id.clone()));
    }
    if !mapping.contains_key(&key("image")) {
        let mut image = serde_yaml::Mapping::new();
        image.insert(key("repository"), serde_yaml::Value::String(app_config.app.id.clone()));
        image.insert(key("tag"), serde_yaml::Value::String("latest".to_string()));
        mapping.insert(key("image"), serde_yaml::Value::Mapping(image));
    }
    if !mapping.contains_key(&key("containerPort")) {
        mapping.insert(key("containerPort"), serde_yaml::Value::Number(serde_yaml::Number::from(app_config.service.port)));
    }
    if !mapping.contains_key(&key("service")) {
        let mut service = serde_yaml::Mapping::new();
        service.insert(key("port"), serde_yaml::Value::Number(serde_yaml::Number::from(app_config.service.port)));
        mapping.insert(key("service"), serde_yaml::Value::Mapping(service));
    }
    if !mapping.contains_key(&key("ingress")) {
        let mut ingress = serde_yaml::Mapping::new();
        ingress.insert(key("enabled"), serde_yaml::Value::Bool(true));
        let mut routing = serde_yaml::Mapping::new();
        routing.insert(key("mode"), serde_yaml::Value::String(app_config.routing.mode.clone()));
        if let Some(ref prefix) = app_config.routing.prefix {
            routing.insert(key("prefix"), serde_yaml::Value::String(prefix.clone()));
        }
        if let Some(ref host) = app_config.routing.host {
            routing.insert(key("host"), serde_yaml::Value::String(host.clone()));
        }
        ingress.insert(key("routing"), serde_yaml::Value::Mapping(routing));
        ingress.insert(key("stripPrefix"), serde_yaml::Value::Bool(app_config.routing.strip_prefix));
        mapping.insert(key("ingress"), serde_yaml::Value::Mapping(ingress));
    }

    let updated = serde_yaml::to_string(&doc).context("Failed to serialize values")?;
    std::fs::write(values_file, updated)
        .with_context(|| format!("Failed to write {}", values_file.display()))?;

    println!("Registered '{}' in {}", helm_key, values_file.display());
    Ok(())
}

/// Remove an app entry from the values file.
pub fn remove_app_from_values(
    values_file: &Path,
    app_config: &AppConfig,
) -> Result<()> {
    let content = std::fs::read_to_string(values_file)
        .with_context(|| format!("Failed to read {}", values_file.display()))?;

    let mut doc: serde_yaml::Value = serde_yaml::from_str(&content)
        .with_context(|| format!("Failed to parse {}", values_file.display()))?;

    let helm_key = app_config.helm_key();

    let apps = doc
        .get_mut("apps")
        .and_then(|v| v.as_mapping_mut())
        .ok_or_else(|| anyhow::anyhow!("values file {} has no 'apps:' map", values_file.display()))?;

    if apps.remove(&serde_yaml::Value::String(helm_key.clone())).is_none() {
        println!("App '{}' not found in {} — nothing to remove.", helm_key, values_file.display());
        return Ok(());
    }

    let updated = serde_yaml::to_string(&doc).context("Failed to serialize values")?;
    std::fs::write(values_file, updated)
        .with_context(|| format!("Failed to write {}", values_file.display()))?;

    println!("Removed '{}' from {}", helm_key, values_file.display());
    Ok(())
}

// ── Helm deploy ───────────────────────────────────────────────────────────────

// Finds and removes releases with the same name deployed to namespaces other than
// env.namespace. These stale releases create duplicate IngressRoutes that compete
// for the same host, causing traffic to land on old pods after a redeploy.
fn warn_and_remove_stale_releases(env: &EnvConfig) -> Result<()> {
    let mut cmd = Command::new("helm");
    cmd.arg("list")
        .arg("--all-namespaces")
        .arg("--filter")
        .arg(format!("^{}$", env.helm_release))
        .arg("--output")
        .arg("json");
    if let Some(ref kc) = env.kubeconfig {
        cmd.arg("--kubeconfig").arg(kc);
    }
    let output = cmd.output().context("Failed to run helm list")?;
    if !output.status.success() {
        return Ok(());
    }
    let json: serde_json::Value =
        serde_json::from_slice(&output.stdout).unwrap_or(serde_json::Value::Array(vec![]));
    let releases = json.as_array().cloned().unwrap_or_default();
    for release in releases {
        let ns = release["namespace"].as_str().unwrap_or("");
        if ns.is_empty() || ns == env.namespace {
            continue;
        }
        eprintln!(
            "WARNING: Found stale release '{}' in namespace '{}' (expected '{}'). Removing it to prevent IngressRoute conflicts...",
            env.helm_release, ns, env.namespace
        );
        let mut rm = Command::new("helm");
        rm.arg("uninstall")
            .arg(&env.helm_release)
            .arg("-n")
            .arg(ns)
            .arg("--wait");
        if let Some(ref kc) = env.kubeconfig {
            rm.arg("--kubeconfig").arg(kc);
        }
        let status = rm.status().context("Failed to run helm uninstall")?;
        if status.success() {
            println!("Removed stale release '{}' from namespace '{}'.", env.helm_release, ns);
        } else {
            eprintln!("WARNING: Failed to remove stale release '{}' from namespace '{}'. Continuing deploy.", env.helm_release, ns);
        }
    }
    Ok(())
}

pub fn run_helm_deploy(env: &EnvConfig) -> Result<()> {
    println!(
        "Deploying helm release '{}' to namespace '{}'...",
        env.helm_release, env.namespace
    );

    if !env.helm_chart_dir.exists() {
        anyhow::bail!(
            "Helm chart not found at {}. Run from the monorepo root or ensure picusbase-apps chart exists.",
            env.helm_chart_dir.display()
        );
    }

    // Detect and remove stale releases of the same release name in other namespaces.
    // This prevents duplicate IngressRoutes competing for the same host.
    warn_and_remove_stale_releases(env)?;

    let mut cmd = Command::new("helm");
    cmd.arg("upgrade")
        .arg("--install")
        .arg(&env.helm_release)
        .arg(&env.helm_chart_dir)
        .arg("-n")
        .arg(&env.namespace)
        .arg("--create-namespace")
        .arg("-f")
        .arg(&env.helm_values_file)
        .arg("--wait")
        .arg("--timeout")
        .arg("10m");

    if let Some(ref kc) = env.kubeconfig {
        cmd.arg("--kubeconfig").arg(kc);
    }

    let status = cmd.status().context("Failed to run helm upgrade")?;
    if !status.success() {
        anyhow::bail!("helm upgrade --install failed");
    }
    println!("Helm deploy complete.");
    Ok(())
}

pub fn run_helm_rollback(env: &EnvConfig, revision: Option<u32>) -> Result<()> {
    let mut cmd = Command::new("helm");
    cmd.arg("rollback").arg(&env.helm_release);
    if let Some(rev) = revision {
        cmd.arg(rev.to_string());
    }
    cmd.arg("-n").arg(&env.namespace).arg("--wait");
    if let Some(ref kc) = env.kubeconfig {
        cmd.arg("--kubeconfig").arg(kc);
    }
    let status = cmd.status().context("Failed to run helm rollback")?;
    if !status.success() {
        anyhow::bail!("helm rollback failed");
    }
    println!("Rollback complete.");
    Ok(())
}

// ── App status / list ─────────────────────────────────────────────────────────

pub fn run_app_status(app_id: &str, env: &EnvConfig) -> Result<()> {
    println!("App status for '{}' in namespace '{}':", app_id, env.namespace);
    let pod_label = format!("app.kubernetes.io/instance={}", app_id);
    let status = env
        .kubectl_base()
        .args([
            "get",
            "pods",
            "-n",
            &env.namespace,
            "-l",
            &pod_label,
            "-o",
            "wide",
        ])
        .status()
        .context("Failed to run kubectl get pods")?;
    if !status.success() {
        anyhow::bail!("kubectl get pods failed");
    }
    Ok(())
}

pub fn run_app_list(env: &EnvConfig) -> Result<()> {
    println!("Apps in namespace '{}':", env.namespace);
    let status = env
        .kubectl_base()
        .args([
            "get",
            "deployments",
            "-n",
            &env.namespace,
            "-l",
            "app.kubernetes.io/name=picus-app",
            "-o",
            "wide",
        ])
        .status()
        .context("Failed to run kubectl get deployments")?;
    if !status.success() {
        anyhow::bail!("kubectl get deployments failed");
    }
    Ok(())
}

// ── Port-forward helper ───────────────────────────────────────────────────────

pub struct PortForward {
    pub child: std::process::Child,
    pub local_port: u16,
}

impl PortForward {
    pub fn open(
        env: &EnvConfig,
        svc_namespace: &str,
        svc_name: &str,
        local_port: u16,
        remote_port: u16,
    ) -> Result<Self> {
        // Check if already open
        if port_is_listening(local_port) {
            println!("Port {} already listening, reusing.", local_port);
            // Return a dummy that won't kill anything on drop
            return Ok(PortForward {
                child: Command::new("true").spawn().context("spawn true")?,
                local_port,
            });
        }

        println!(
            "Opening port-forward {} -> {}:{} ...",
            local_port, svc_name, remote_port
        );

        let mut cmd = env.kubectl_base();
        cmd.args([
            "port-forward",
            "-n",
            svc_namespace,
            &format!("svc/{}", svc_name),
            &format!("{}:{}", local_port, remote_port),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null());

        let child = cmd.spawn().context("Failed to start kubectl port-forward")?;

        // Wait for port to open
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while !port_is_listening(local_port) {
            if std::time::Instant::now() >= deadline {
                anyhow::bail!(
                    "Timeout waiting for port-forward on localhost:{}",
                    local_port
                );
            }
            thread::sleep(Duration::from_millis(500));
        }
        println!("Port-forward ready on localhost:{}", local_port);
        Ok(PortForward { child, local_port })
    }
}

impl Drop for PortForward {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn port_is_listening(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(
        &format!("127.0.0.1:{}", port).parse().unwrap(),
        Duration::from_millis(200),
    )
    .is_ok()
}

// ── Database migration ────────────────────────────────────────────────────────

pub struct MigrationContext {
    pub db_url: String,
    pub project_ref: String,
    pub role_name: String,
    pub shard_id: String,
}

pub fn run_migrations(
    app_config: &AppConfig,
    config_path: &Path,
    project_ref: &str,
    env: &EnvConfig,
    only_files: &[String],
) -> Result<()> {
    let migrations = app_config.migrations.as_ref().ok_or_else(|| {
        anyhow::anyhow!(
            "No [migrations] section in app.toml — nothing to migrate"
        )
    })?;

    let app_dir = AppConfig::app_dir(config_path);
    let migrations_dir = app_dir.join(&migrations.dir);

    if !migrations_dir.exists() {
        anyhow::bail!(
            "Migrations directory not found: {}",
            migrations_dir.display()
        );
    }

    let admin_user =
        std::env::var("ADMIN_USER").unwrap_or_else(|_| "admin".to_string());
    let admin_password =
        std::env::var("ADMIN_PASSWORD").unwrap_or_else(|_| "strongpassword".to_string());
    let db_name =
        std::env::var("DB_NAME").unwrap_or_else(|_| "picus_dev".to_string());
    let pf_port = env.pgdog_pf_port;
    let db_url = format!(
        "postgresql://{}:{}@127.0.0.1:{}/{}?sslmode=disable",
        admin_user, admin_password, pf_port, db_name
    );

    println!("======================================");
    println!("  saas_ops migration");
    println!("  project_ref : {}", project_ref);
    println!("  role        : role_{}", project_ref);
    println!("  env         : {}", if env.is_local { "local" } else { "product" });
    println!("======================================");

    // Open port-forward
    let _pf = PortForward::open(
        env,
        &env.picusbase_namespace,
        "pgdog-proxy",
        pf_port,
        6432,
    )?;

    // [1] Validate DB connection
    println!("[1/5] Validating DB connection...");
    psql_exec(&db_url, "SELECT 1", None)?;

    // [2] Get shard_id for project_ref
    println!("[2/5] Resolving shard for project '{}'...", project_ref);
    let shard_query = format!(
        "SELECT shard_id FROM system.schema_routes WHERE schema_name = '{}' LIMIT 1;",
        project_ref
    );
    let shard_out = psql_query(&db_url, &shard_query)?;
    if shard_out.trim().is_empty() {
        anyhow::bail!(
            "Project '{}' not found in system.schema_routes. Ensure the project is provisioned.",
            project_ref
        );
    }
    let shard_id = shard_out.trim().to_string();
    println!("  project on shard: {}", shard_id);

    // [3] Validate role exists
    let role_name = format!("role_{}", project_ref);
    let role_query = format!(
        "SELECT 1 FROM pg_roles WHERE rolname = '{}';",
        role_name
    );
    let role_out = psql_query(&db_url, &role_query)?;
    if role_out.trim().is_empty() {
        anyhow::bail!(
            "Role '{}' not found. PicusBase must finish provisioning before running migrations.",
            role_name
        );
    }
    println!("  role '{}' confirmed.", role_name);

    // [4] Collect migration files
    let files_to_run = collect_migration_files(&migrations_dir, migrations, only_files)?;
    println!("[3/5] Running {} migration file(s)...", files_to_run.len());

    let set_path = format!(
        "SET search_path = \"{}\", public, extensions;",
        project_ref
    );

    for file in &files_to_run {
        println!("  Applying: {}", file.display());
        let result = psql_exec_file(&db_url, &set_path, file);
        match result {
            Ok(_) => {}
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("already exists") || msg.contains("duplicate") {
                    eprintln!("  WARN: {} (already exists, continuing)", file.display());
                } else {
                    return Err(e);
                }
            }
        }
    }

    // [5] Rebind schema ownership
    let schema_name = migrations
        .schema_name
        .as_deref()
        .unwrap_or("saas_ops");

    println!(
        "[4/5] Rebinding schema '{}' to '{}'...",
        schema_name, role_name
    );

    let rebind_sql = format!(
        r#"
SET search_path = "{project_ref}", public, extensions;

ALTER SCHEMA {schema} OWNER TO "{role}";
REVOKE ALL ON SCHEMA {schema} FROM PUBLIC;
GRANT USAGE ON SCHEMA {schema} TO "{role}";
GRANT ALL ON ALL TABLES IN SCHEMA {schema} TO "{role}";
GRANT ALL ON ALL SEQUENCES IN SCHEMA {schema} TO "{role}";
ALTER DEFAULT PRIVILEGES IN SCHEMA {schema} GRANT ALL ON TABLES TO "{role}";
ALTER DEFAULT PRIVILEGES IN SCHEMA {schema} GRANT ALL ON SEQUENCES TO "{role}";

INSERT INTO system.schema_routes (project_ref, schema_name, shard_id, status)
VALUES ('{project_ref}', '{schema}', {shard_id}, 'active')
ON CONFLICT (schema_name) DO UPDATE
  SET project_ref = EXCLUDED.project_ref,
      shard_id    = EXCLUDED.shard_id,
      status      = 'active',
      updated_at  = now();
"#,
        project_ref = project_ref,
        schema = schema_name,
        role = role_name,
        shard_id = shard_id,
    );

    psql_exec(&db_url, &rebind_sql, None)?;
    println!("  Schema ownership and schema_routes updated.");

    // [6] Setup background_jobs (if configured)
    if migrations.setup_background_jobs {
        println!("[5/5] Setting up background_jobs table...");
        let bg_sql = format!(
            r#"
CREATE TABLE IF NOT EXISTS "{project_ref}".background_jobs (
    id uuid DEFAULT gen_random_uuid() NOT NULL,
    status text DEFAULT 'pending'::text NOT NULL,
    wasm_module text NOT NULL,
    function_id text,
    target_kind text,
    topic text,
    params jsonb DEFAULT '{{}}'::jsonb,
    priority integer DEFAULT 0,
    version_id uuid,
    artifact_checksum text,
    runtime_class text,
    allowed_capabilities text[] DEFAULT '{{}}'::text[] NOT NULL,
    policy_id uuid,
    policy_timeout_ms integer,
    attempt_count integer DEFAULT 0 NOT NULL,
    max_retries integer DEFAULT 3 NOT NULL,
    retry_backoff_ms integer DEFAULT 1000 NOT NULL,
    worker_id uuid,
    result jsonb,
    error_message text,
    error_class text,
    created_at timestamp with time zone DEFAULT now(),
    updated_at timestamp with time zone DEFAULT now(),
    scheduled_at timestamp with time zone DEFAULT now(),
    started_at timestamp with time zone,
    completed_at timestamp with time zone,
    workflow_run_id uuid,
    workflow_node_id uuid,
    parent_job_id uuid
);

SET search_path = "{project_ref}", public, extensions;
ALTER TABLE "{project_ref}".background_jobs REPLICA IDENTITY FULL;
GRANT SELECT, INSERT, UPDATE ON "{project_ref}".background_jobs TO "{role}";
"#,
            project_ref = project_ref,
            role = role_name,
        );
        match psql_exec(&db_url, &bg_sql, None) {
            Ok(_) => println!("  background_jobs table ready."),
            Err(e) => eprintln!("  WARN: background_jobs setup: {}", e),
        }
    }

    println!();
    println!("======================================");
    println!("  Migration complete.");
    println!("  Schema '{}' ready for project '{}'.", schema_name, project_ref);
    println!("======================================");
    Ok(())
}

fn collect_migration_files(
    migrations_dir: &Path,
    migrations: &crate::app_config::MigrationsSection,
    only_files: &[String],
) -> Result<Vec<PathBuf>> {
    let filter = if !only_files.is_empty() {
        only_files.to_vec()
    } else {
        migrations.files.clone()
    };

    if !filter.is_empty() {
        // Use explicit list
        let mut result = Vec::new();
        for f in &filter {
            let path = migrations_dir.join(f);
            if !path.exists() {
                anyhow::bail!("Migration file not found: {}", path.display());
            }
            result.push(path);
        }
        return Ok(result);
    }

    // Glob all [0-9]*.sql files sorted
    let mut entries: Vec<PathBuf> = std::fs::read_dir(migrations_dir)
        .with_context(|| format!("Cannot read migrations dir {}", migrations_dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension().and_then(|e| e.to_str()) == Some("sql")
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.chars().next().map(|c| c.is_ascii_digit()).unwrap_or(false))
                    .unwrap_or(false)
        })
        .collect();
    entries.sort();
    Ok(entries)
}

fn psql_exec(db_url: &str, sql: &str, _file: Option<&Path>) -> Result<()> {
    use std::io::Write;

    let mut child = Command::new("psql")
        .arg(db_url)
        .arg("-v")
        .arg("ON_ERROR_STOP=1")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("Failed to spawn psql (is psql in PATH?)")?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(sql.as_bytes()).context("Failed to write SQL to psql stdin")?;
    }

    let output = child.wait_with_output().context("Failed to wait for psql")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("psql error: {}\nSQL:\n{}", stderr.trim(), sql);
    }
    Ok(())
}

fn psql_exec_file(db_url: &str, set_path_sql: &str, file: &Path) -> Result<()> {
    let output = Command::new("psql")
        .arg(db_url)
        .arg("-v")
        .arg("ON_ERROR_STOP=1")
        .arg("-c")
        .arg(set_path_sql)
        .arg("-f")
        .arg(file)
        .output()
        .context("Failed to run psql for file")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("psql error on {}: {}", file.display(), stderr.trim());
    }
    Ok(())
}

fn psql_query(db_url: &str, sql: &str) -> Result<String> {
    let output = Command::new("psql")
        .arg(db_url)
        .arg("-t")
        .arg("-A")
        .arg("-c")
        .arg(sql)
        .output()
        .context("Failed to run psql query")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("psql query error: {}", stderr.trim());
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

// ── Functions deploy ──────────────────────────────────────────────────────────

pub fn run_functions_deploy(
    app_config: &AppConfig,
    config_path: &Path,
    project_ref: &str,
    env: &EnvConfig,
    single_function: Option<&str>,
    picusbase_cli: &str,
) -> Result<()> {
    let compute = app_config.compute.as_ref().ok_or_else(|| {
        anyhow::anyhow!("No [compute] section in app.toml — no functions to deploy")
    })?;

    let app_dir = AppConfig::app_dir(config_path);
    let functions_dir = compute
        .functions_dir
        .as_ref()
        .map(|d| app_dir.join(d))
        .unwrap_or_else(|| app_dir.join("functions"));

    if !functions_dir.exists() {
        anyhow::bail!(
            "Functions directory not found: {}",
            functions_dir.display()
        );
    }

    let control_plane_url = format!("http://127.0.0.1:{}/api", env.compute_cp_port);

    // Port-forward compute control plane
    let _cp_pf = PortForward::open(
        env,
        &env.picusbase_namespace,
        "picusbase-compute-control-plane",
        env.compute_cp_port,
        4080,
    )?;

    if let Some(func_key) = single_function {
        println!("Deploying single function: {}", func_key);
        deploy_single_function(
            &functions_dir,
            func_key,
            project_ref,
            &control_plane_url,
            picusbase_cli,
        )?;
    } else {
        // Deploy all functions
        let all_domains: Vec<String> = {
            let mut v = compute.provisioning_functions
                .iter()
                .map(|_| "provisioning".to_string())
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .collect::<Vec<_>>();
            v.extend(compute.business_domains.clone());
            // dedupe: just use a set
            compute
                .business_domains
                .iter()
                .map(|d| d.clone())
                .chain(std::iter::once("provisioning".to_string()))
                .collect::<std::collections::HashSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
        };

        for domain in &all_domains {
            let domain_dir = functions_dir.join(domain);
            if !domain_dir.is_dir() {
                continue;
            }
            println!("--- Deploying domain: {} ---", domain);
            for entry in std::fs::read_dir(&domain_dir)
                .with_context(|| format!("Cannot read {}", domain_dir.display()))?
            {
                let entry = entry?;
                let func_dir = entry.path();
                if func_dir.join("picus.toml").exists() {
                    let status = Command::new(picusbase_cli)
                        .arg("deploy")
                        .arg("functions")
                        .arg("--path")
                        .arg(&func_dir)
                        .arg("--url")
                        .arg(&control_plane_url)
                        .arg("--project-ref")
                        .arg(project_ref)
                        .status()
                        .context("Failed to run picusbase deploy functions")?;
                    if !status.success() {
                        eprintln!(
                            "Warning: deploy failed for {}",
                            func_dir.display()
                        );
                    }
                }
            }
        }
    }

    // Run post-deploy SQL if configured
    if let Some(ref sql_path_str) = compute.post_deploy_sql {
        let sql_file = app_dir.join(sql_path_str);
        if sql_file.exists() {
            println!("Running post-deploy SQL: {}", sql_file.display());
            // Open db port-forward
            let _db_pf = PortForward::open(
                env,
                &env.picusbase_namespace,
                "pgdog-proxy",
                env.pgdog_pf_port,
                6432,
            )?;
            let admin_user =
                std::env::var("ADMIN_USER").unwrap_or_else(|_| "admin".to_string());
            let admin_password =
                std::env::var("ADMIN_PASSWORD").unwrap_or_else(|_| "strongpassword".to_string());
            let db_name =
                std::env::var("DB_NAME").unwrap_or_else(|_| "picus_dev".to_string());
            let db_url = format!(
                "postgresql://{}:{}@127.0.0.1:{}/{}?sslmode=disable",
                admin_user, admin_password, env.pgdog_pf_port, db_name
            );
            let set_path = format!(
                "SET search_path = \"{}\", public, extensions;",
                project_ref
            );
            match psql_exec_file(&db_url, &set_path, &sql_file) {
                Ok(_) => println!("Post-deploy SQL complete."),
                Err(e) => eprintln!("Warning: post-deploy SQL error: {}", e),
            }
        }
    }

    println!("Functions deploy complete.");
    Ok(())
}

fn deploy_single_function(
    functions_dir: &Path,
    func_key: &str,
    project_ref: &str,
    control_plane_url: &str,
    picusbase_cli: &str,
) -> Result<()> {
    // Search all domain subdirectories
    for entry in std::fs::read_dir(functions_dir)
        .with_context(|| format!("Cannot read {}", functions_dir.display()))?
    {
        let entry = entry?;
        let domain_dir = entry.path();
        if !domain_dir.is_dir() {
            continue;
        }
        let func_dir = domain_dir.join(func_key);
        if func_dir.join("picus.toml").exists() {
            let status = Command::new(picusbase_cli)
                .arg("deploy")
                .arg("functions")
                .arg("--path")
                .arg(&func_dir)
                .arg("--url")
                .arg(control_plane_url)
                .arg("--project-ref")
                .arg(project_ref)
                .status()
                .context("Failed to run picusbase deploy functions")?;
            if !status.success() {
                anyhow::bail!("Deploy failed for function '{}'", func_key);
            }
            return Ok(());
        }
    }
    anyhow::bail!(
        "Function '{}' not found under {}",
        func_key,
        functions_dir.display()
    )
}

// ── Full start orchestration ──────────────────────────────────────────────────

pub struct StartOptions {
    pub skip_migrate: bool,
    pub skip_functions: bool,
    pub skip_sync: bool,
    pub skip_build: bool,
    pub only_functions: bool,
    pub extra_build_args: Vec<(String, String)>,
    pub force_clean: bool,
}

pub fn run_start(
    app_config: &AppConfig,
    config_path: &Path,
    project_ref: &str,
    tag: &str,
    env: &EnvConfig,
    opts: &StartOptions,
    picusbase_cli: &str,
) -> Result<()> {
    if opts.only_functions {
        println!("=== ONLY_FUNCTIONS mode ===");
        run_functions_deploy(app_config, config_path, project_ref, env, None, picusbase_cli)?;
        return Ok(());
    }

    let has_migrations = app_config.migrations.is_some();
    let has_compute = app_config.compute.is_some();

    // Ensure the app has a complete entry in the values file before any step writes to it.
    register_app_in_values(&env.helm_values_file, app_config)?;

    // Step 1: migrate
    if !opts.skip_migrate && has_migrations {
        println!();
        println!("=== Step 1/5: Database Migrations ===");
        run_migrations(app_config, config_path, project_ref, env, &[])?;
    } else if opts.skip_migrate {
        println!("SKIP_MIGRATE — skipping migrations.");
    }

    // Step 2: functions deploy
    if !opts.skip_functions && has_compute {
        println!();
        println!("=== Step 2/5: Deploy Functions ===");
        run_functions_deploy(app_config, config_path, project_ref, env, None, picusbase_cli)?;
    } else if opts.skip_functions {
        println!("SKIP_FUNCTIONS — skipping functions deploy.");
    }

    // Step 3: values_patch (start.values_patch)
    if let Some(ref start_cfg) = app_config.start {
        if !start_cfg.values_patch.is_empty() {
            println!();
            println!("=== Step 3/5: Patching values ===");
            apply_values_patch(&env.helm_values_file, &start_cfg.values_patch, project_ref)?;
        }
    }

    // Step 4: build
    if !opts.skip_build {
        println!();
        println!("=== Step 4/5: Build Image ===");
        run_build(
            app_config,
            config_path,
            tag,
            env,
            &opts.extra_build_args,
            opts.force_clean,
        )?;
    } else {
        println!("SKIP_BUILD — skipping image build.");
    }

    // Step 5: helm deploy
    println!();
    println!("=== Step 5/5: Helm Deploy ===");
    // Create namespace first
    let _ = env
        .kubectl_base()
        .args(["create", "namespace", &env.namespace, "--dry-run=client", "-o", "yaml"])
        .output();
    let _ = env
        .kubectl_base()
        .args(["apply", "-f", "-"])
        .stdin(Stdio::piped())
        .spawn();

    copy_registry_secret(env).unwrap_or_else(|e| eprintln!("Warning: {}", e));
    copy_internal_token_secret(app_config, env).unwrap_or_else(|e| eprintln!("Warning: {}", e));

    patch_helm_values(&env.helm_values_file, app_config, tag)?;
    run_helm_deploy(env)?;

    println!();
    println!("======================================");
    println!("  Start complete.");
    println!("  app_id      : {}", app_config.app.id);
    println!("  project_ref : {}", project_ref);
    println!("  tag         : {}", tag);
    println!("======================================");
    Ok(())
}

fn apply_values_patch(
    values_file: &Path,
    patches: &std::collections::HashMap<String, String>,
    project_ref: &str,
) -> Result<()> {
    let content = std::fs::read_to_string(values_file)
        .with_context(|| format!("Failed to read {}", values_file.display()))?;
    let mut doc: serde_yaml::Value = serde_yaml::from_str(&content)
        .with_context(|| format!("Failed to parse {}", values_file.display()))?;

    for (dot_path, value_template) in patches {
        let value = value_template.replace("{project_ref}", project_ref);
        set_yaml_dot_path(&mut doc, dot_path, &value)?;
        println!("  {} = {}", dot_path, value);
    }

    let updated = serde_yaml::to_string(&doc).context("Failed to serialize values")?;
    std::fs::write(values_file, updated)
        .with_context(|| format!("Failed to write {}", values_file.display()))?;
    Ok(())
}

fn set_yaml_dot_path(
    doc: &mut serde_yaml::Value,
    dot_path: &str,
    value: &str,
) -> Result<()> {
    let parts: Vec<&str> = dot_path.split('.').collect();
    let mut current = doc;
    for (i, part) in parts.iter().enumerate() {
        if i == parts.len() - 1 {
            if let Some(map) = current.as_mapping_mut() {
                map.insert(
                    serde_yaml::Value::String(part.to_string()),
                    serde_yaml::Value::String(value.to_string()),
                );
            }
        } else {
            let key = serde_yaml::Value::String(part.to_string());
            if let Some(map) = current.as_mapping_mut() {
                let entry = map
                    .entry(key)
                    .or_insert_with(|| serde_yaml::Value::Mapping(serde_yaml::Mapping::new()));
                current = entry;
            } else {
                anyhow::bail!("Cannot traverse YAML at path component '{}'", part);
            }
        }
    }
    Ok(())
}
