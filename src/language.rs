//! Language adapter abstraction for guest Compute Function authoring.
//!
//! This module formalizes what the Picus CLI does on behalf of a guest
//! language — scaffold, build or package, and locate the resulting publishable
//! artifact — so evolving lane support does not require editing every command
//! in `main.rs`.
//!
//! Source of truth for lane contracts lives in
//! `docs-platform/01-architecture/04-compute-architecture.md`.
//!
//! Rust, JavaScript, and Python adapters are implemented here so the CLI can
//! keep language-specific build logic in one place while the rest of the
//! command surface stays language-agnostic.

use crate::bundler::{self, BundleKind};
use crate::config::PicusConfig;
use anyhow::{anyhow, bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Docker Desktop's own configured egress proxy (Settings → Resources →
/// Proxies), if any — the `HttpProxy`/`HttpsProxy`/`NoProxy` fields `docker
/// info` reports, not the host shell's `HTTP_PROXY` env var, which that
/// setting doesn't populate. Best-effort: any failure to run/parse `docker
/// info` just means no proxy vars get added, same as if none were
/// configured. Values come back without a scheme (`host:port`); callers
/// that treat them as a proxy URL need `http://` prepended, so it's added
/// here once rather than by every caller.
fn docker_configured_proxy_env() -> Vec<(&'static str, String)> {
    let Ok(output) = Command::new("docker").args(["info", "--format", "{{json .}}"]).output() else {
        return Vec::new();
    };
    let Ok(info) = serde_json::from_slice::<serde_json::Value>(&output.stdout) else {
        return Vec::new();
    };
    let mut vars = Vec::new();
    for (json_field, env_var, needs_scheme) in [
        ("HttpProxy", "HTTP_PROXY", true),
        ("HttpsProxy", "HTTPS_PROXY", true),
        ("NoProxy", "NO_PROXY", false),
    ] {
        if let Some(value) = info.get(json_field).and_then(|v| v.as_str()).filter(|v| !v.is_empty()) {
            let value = if needs_scheme && !value.contains("://") {
                format!("http://{value}")
            } else {
                value.to_string()
            };
            vars.push((env_var, value));
        }
    }
    vars
}

/// Rust functions are built in a Linux container: fail early, with the reason, when Docker is not usable.
pub fn require_docker() -> Result<()> {
    let output = Command::new("docker")
        .args(["version", "--format", "{{.Server.Version}}"])
        .output()
        .context("`docker` was not found on PATH (needed to build Rust functions)")?;
    if !output.status.success() {
        bail!(
            "Docker is not running (needed to build Rust functions): {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Supported guest languages recognized by the CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Rust,
    JavaScript,
    Python,
}

impl Language {
    pub fn id(self) -> &'static str {
        match self {
            Language::Rust => "rust",
            Language::JavaScript => "javascript",
            Language::Python => "python",
        }
    }

    pub fn parse(value: &str) -> Result<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "rust" | "rs" => Ok(Language::Rust),
            "javascript" | "js" => Ok(Language::JavaScript),
            "python" | "py" => Ok(Language::Python),
            other => Err(anyhow!(
                "Unknown language '{}'. Supported: rust, javascript, python. JavaScript authoring uses src/index.mjs and is normalized into the javascript-v1 bundle contract at package time; see docs-platform/01-architecture/04-compute-architecture.md for lane-specific contracts.",
                other
            )),
        }
    }
}

impl std::fmt::Display for Language {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.id())
    }
}

/// Result of a successful build or package step: the path to the artifact that
/// should be uploaded to the platform.
#[derive(Debug, Clone)]
pub struct BuildOutput {
    pub artifact: PathBuf,
}

/// Adapter contract that every guest language implementation must satisfy.
///
/// Wasm-producing adapters must conform to ABI v2 documented in
/// `docs-platform/01-architecture/04-compute-architecture.md`; bundle-producing adapters must
/// emit the publishable artifact shape required by their runtime class.
pub trait LanguageAdapter {
    /// Stable identifier for this adapter.
    fn language(&self) -> Language;

    /// Detect whether this adapter recognizes a function directory based
    /// on its layout (e.g. presence of `Cargo.toml`).
    fn detect(&self, dir: &Path) -> bool;

    /// Build or package the function directory and return the path to the
    /// produced artifact. Implementations are responsible for ensuring any
    /// required toolchain or staging layout is set up and for honoring the
    /// shared build cache convention.
    fn build(&self, dir: &Path, release: bool, shared_target: &Path) -> Result<BuildOutput>;

    /// Build/package with an explicit runtime class when the artifact envelope
    /// itself must carry the class (bundle lanes). Wasm-producing adapters can
    /// ignore this because their runtime class is upload metadata.
    fn build_with_runtime_class(
        &self,
        dir: &Path,
        release: bool,
        shared_target: &Path,
        _runtime_class: Option<&str>,
    ) -> Result<BuildOutput> {
        self.build(dir, release, shared_target)
    }

    /// Cheap checks that a build would fail on, run without building anything so a batch deploy
    /// can reject a bad function before it publishes the first one.
    fn preflight(&self, _dir: &Path) -> Result<()> {
        Ok(())
    }
}

/// Detect which adapter applies to a function directory. Returns the
/// first matching adapter or an error listing every adapter we tried.
///
/// Order matters: more-specific adapters first so a Python function dir
/// (Cargo.toml + src/handler.py) is not misclassified as plain Rust.
pub fn detect_language(dir: &Path) -> Result<Language> {
    if PythonAdapter.detect(dir) {
        return Ok(Language::Python);
    }
    if JavaScriptAdapter.detect(dir) {
        return Ok(Language::JavaScript);
    }
    if RustAdapter.detect(dir) {
        return Ok(Language::Rust);
    }
    Err(anyhow!(
        "Could not detect guest language for {}: no Cargo.toml, src/index.mjs, or src/handler.py found.",
        dir.display()
    ))
}

/// Get the adapter implementation for a given language.
pub fn adapter_for(lang: Language) -> Box<dyn LanguageAdapter> {
    match lang {
        Language::Rust => Box::new(RustAdapter),
        Language::JavaScript => Box::new(JavaScriptAdapter),
        Language::Python => Box::new(PythonAdapter),
    }
}

// ---------------------------------------------------------------------------
// Rust adapter.
// ---------------------------------------------------------------------------

pub struct RustAdapter;

impl LanguageAdapter for RustAdapter {
    fn language(&self) -> Language {
        Language::Rust
    }

    fn detect(&self, dir: &Path) -> bool {
        dir.join("Cargo.toml").exists()
    }

    fn build(&self, dir: &Path, release: bool, shared_target: &Path) -> Result<BuildOutput> {
        self.build_with_runtime_class(dir, release, shared_target, None)
    }

    fn preflight(&self, dir: &Path) -> Result<()> {
        if !dir.join("src").join("bin").join("native_worker.rs").exists() {
            bail!("missing src/bin/native_worker.rs (required native-v1 entrypoint)");
        }
        docker_mount_root(&dir.canonicalize().with_context(|| format!("Failed to resolve {}", dir.display()))?).map(|_| ())
    }

    /// native-v1 only — the wasm32/wasm-core-v1 build path has been retired
    /// entirely (gVisor Dispatcher has no execution path for wasm-core-v1,
    /// see `services/compute-dispatcher/src/main.rs::resolve_guest_image`;
    /// every Rust Function now targets native-v1, matching
    /// `platform-functions/entities-data`'s migration). Requires the
    /// function dir to have a `src/bin/native_worker.rs` entrypoint (a small
    /// stdin/stdout JSON loop calling the crate's `HttpHandler`, see
    /// `platform-functions/entities-data/src/bin/native_worker.rs` for the
    /// reference shape) — the CLI does not scaffold one automatically.
    ///
    /// Cross-compiles inside a Linux container (same base image
    /// `infrastructure/docker/Dockerfile.rust-service` builds services with)
    /// rather than the host `cargo`, because the CLI may run on macOS/Windows
    /// but the compiled binary must run inside a Linux gVisor guest Pod
    /// (`services/compute-native-shim` downloads and execs it as-is, no
    /// interpreter). Requires `docker` on PATH.
    fn build_with_runtime_class(
        &self,
        dir: &Path,
        _release: bool,
        shared_target: &Path,
        runtime_class: Option<&str>,
    ) -> Result<BuildOutput> {
        let runtime_class = runtime_class.unwrap_or("native-v1");
        if runtime_class != "native-v1" {
            bail!("Rust adapter only supports runtime_class 'native-v1', got '{runtime_class}'");
        }

        if !dir.join("src").join("bin").join("native_worker.rs").exists() {
            bail!(
                "Rust function {} is missing src/bin/native_worker.rs (required native-v1 entrypoint — see platform-functions/entities-data/src/bin/native_worker.rs for the reference shape)",
                dir.display()
            );
        }

        let abs_dir = dir
            .canonicalize()
            .with_context(|| format!("Failed to resolve {}", dir.display()))?;

        // Mount the smallest directory that holds the function and everything its Cargo.toml
        // reaches by relative path (path dependencies, a parent cargo workspace), preserving the
        // function's position under it as the container workdir, so Cargo resolves those paths
        // exactly like it does on the host. A function with only registry dependencies mounts
        // just its own directory.
        let workspace_root = docker_mount_root(&abs_dir)?;
        let rel_dir = abs_dir.strip_prefix(&workspace_root).with_context(|| {
            format!(
                "{} is not under workspace root {}",
                abs_dir.display(),
                workspace_root.display()
            )
        })?;
        let container_workdir = Path::new("/workspace").join(rel_dir);
        // Build output goes to the shared target dir (one per `functions/` root), not under the
        // mount root: that root is the function's own directory when it has no path
        // dependencies, so a per-root target would rebuild every dependency for every function.
        std::fs::create_dir_all(shared_target)
            .with_context(|| format!("Failed to create {}", shared_target.display()))?;
        const CONTAINER_TARGET: &str = "/native-target";

        const RUST_IMAGE: &str = "rust:1.95.0-slim-trixie";
        // `rustup` searches UPWARD from the container's cwd for a
        // `rust-toolchain(.toml)` file, and `-v {workspace_root}:/workspace`
        // mounts everything from `workspace_root` down — so ANY
        // `rust-toolchain.toml` sitting above the function dir on the HOST
        // (even one that has nothing to do with this function, or this repo:
        // confirmed the hard way with one sitting one level above the
        // `picusbase`/bundle checkouts, containing `channel = "stable"` plus
        // `rustfmt`/`clippy`/`rust-analyzer`/`wasm32-unknown-unknown` — an
        // IDE convenience file, not a build input) gets picked up and
        // overrides the image's own pinned toolchain. Every one of these
        // `--rm` containers is ephemeral (no persisted `$RUSTUP_HOME`), so
        // `channel = "stable"` being a floating target means EVERY function
        // build re-resolves + re-downloads that "stable" toolchain (8
        // components) from scratch — measured as the dominant share of each
        // function's build time, not the actual `cargo build` (which reports
        // its own time separately and was consistently 2-3s once the
        // toolchain was actually in place). `RUSTUP_TOOLCHAIN` overrides any
        // toolchain-file lookup outright, pinning to exactly what
        // `${RUST_IMAGE}` already ships — zero network, zero re-resolution,
        // regardless of what any ancestor directory on any given machine
        // happens to contain.
        let toolchain_version = RUST_IMAGE
            .rsplit(':')
            .next()
            .and_then(|tag| tag.split('-').next())
            .unwrap_or("stable");

        let mut cmd = Command::new("docker");
        cmd.args(["run", "--rm"]);
        // `docker run` with no `--platform` resolves the image to the HOST's
        // architecture (Docker Desktop's default), so on an Apple Silicon
        // dev machine this silently cross-"compiles" an arm64 Linux ELF —
        // correct OS, wrong CPU arch for any real deploy target, which is
        // x86_64 almost everywhere except a local kind cluster running under
        // Docker Desktop on the SAME Apple Silicon host (arm64 there too, by
        // coincidence, which is exactly why this went unnoticed locally).
        // Bug caught for real 2026-09-25: every native-v1 Rust function in a
        // bundle built on an arm64 Mac and deployed to the (x86_64) product
        // VM failed guest cold-start with "Exec format error" — confirmed by
        // inspecting the packaged artifact's ELF header directly
        // (`ARM aarch64` inside a bundle meant for an x86_64 node).
        // `PICUS_NATIVE_BUILD_PLATFORM` overrides the default (host-arch)
        // resolution when the deploy target's architecture is known and
        // differs from the machine running this build — set by
        // `picus-erp/deploy.sh` from `deploy/product.env`. Left unset,
        // behavior is unchanged from before this fix.
        if let Ok(platform) = std::env::var("PICUS_NATIVE_BUILD_PLATFORM") {
            if !platform.trim().is_empty() {
                cmd.args(["--platform", platform.trim()]);
            }
        }
        cmd.arg("-v")
            .arg(format!("{}:/workspace", workspace_root.display()))
            .arg("-v")
            .arg(format!("{}:{CONTAINER_TARGET}", shared_target.display()))
            // Crates.io downloads survive between function builds instead of being fetched again
            // by every one of the `--rm` containers.
            .args(["-v", "picusbase-cargo-registry:/usr/local/cargo/registry"])
            .arg("-w")
            .arg(container_workdir.display().to_string())
            .args(["-e", &format!("CARGO_TARGET_DIR={CONTAINER_TARGET}/native-build")])
            .args(["-e", &format!("RUSTUP_TOOLCHAIN={toolchain_version}")]);
        // Forward real per-environment values into the build when the CLI's own environment has
        // them set (from <env>.env via deploy.sh), so a function reading the matching
        // `option_env!(...)` (the compile-time convention every bundle function uses for values
        // it can't discover at runtime — see e.g. console-api's `verify_internal_token`) gets the
        // real cluster value baked in instead of silently falling back to its own hardcoded
        // local-dev default. Left unset (local kind's dev.env), behavior is unchanged — each
        // function's own default already matches local kind's dev environment. Add a name here
        // whenever a function needs one more of these; three real bugs from exactly this gap so
        // far: `provision_tenant` shipped `"local-dev-internal-service-token"` baked in (401 the
        // moment PICUS_FETCH_ALLOWLIST was fixed, 2026-09-25), then `"...picusbase.localhost"`
        // baked into every new tenant's domain (2026-09-25), then a local-kind-only
        // `SOURCE_PROJECT_REF` default that silently registered ZERO business functions for every
        // tenant provisioned on product (2026-09-26 — every prior tenant had only the built-in
        // `entities-data` function, nothing else).
        for var in ["PICUS_INTERNAL_SERVICE_TOKEN", "ERP_TENANT_DOMAIN_SUFFIX", "ERP_SOURCE_PROJECT_REF"] {
            if let Ok(value) = std::env::var(var) {
                if !value.trim().is_empty() {
                    cmd.args(["-e", &format!("{var}={}", value.trim())]);
                }
            }
        }
        // Belt-and-suspenders: the toolchain itself no longer needs network
        // (see above), but a function's own crates.io dependencies still do
        // on their first build in a fresh container — same proxy-gated-
        // Docker-Desktop reasoning as before applies to those.
        for (var, value) in docker_configured_proxy_env() {
            cmd.args(["-e", &format!("{var}={value}")]);
        }
        let status = cmd
            .arg(RUST_IMAGE)
            .args(["cargo", "build", "--release", "--bin", "native_worker"])
            .status()
            .context("Failed to invoke docker (required to cross-compile the native-v1 Rust binary for Linux)")?;

        if !status.success() {
            bail!(
                "docker cargo build failed for native-v1 binary in {}",
                dir.display()
            );
        }

        let binary_path = shared_target
            .join("native-build")
            .join("release")
            .join("native_worker");
        if !binary_path.exists() {
            bail!(
                "Build succeeded but native_worker binary was not found at {}",
                binary_path.display()
            );
        }

        let function_id = read_function_id(dir)
            .context("Rust function dir must have picus.toml with [function].id")?;
        let artifact_dir = shared_target.join("native-artifacts");
        std::fs::create_dir_all(&artifact_dir)
            .with_context(|| format!("Failed to create {}", artifact_dir.display()))?;
        let artifact = artifact_dir.join(format!("{}.tar.gz", function_id));
        let allowed_capabilities = PicusConfig::load_from_dir(dir)?.allowed_capabilities()?;

        bundler::package_native_binary_bundle(&binary_path, &artifact, &allowed_capabilities)
            .with_context(|| format!("Failed to package native-v1 bundle from {}", dir.display()))?;

        Ok(BuildOutput { artifact })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceLayout {
    Src,
}

fn javascript_source_layout(dir: &Path) -> Option<SourceLayout> {
    dir.join("src")
        .join("index.mjs")
        .exists()
        .then_some(SourceLayout::Src)
}

fn python_source_layout(dir: &Path) -> Option<SourceLayout> {
    dir.join("src")
        .join("handler.py")
        .exists()
        .then_some(SourceLayout::Src)
}

// ---------------------------------------------------------------------------
// JavaScript adapter — bundle the active `javascript-v1` lane contract.
//
// Authoring layout is source-root first: the function dir SHOULD contain
// `picus.toml` and `src/index.mjs`. The adapter normalizes that authoring
// layout into the runtime bundle contract and emits a deterministic `.tar.gz`
// artifact under the shared target dir.
// ---------------------------------------------------------------------------

pub struct JavaScriptAdapter;

impl LanguageAdapter for JavaScriptAdapter {
    fn language(&self) -> Language {
        Language::JavaScript
    }

    fn detect(&self, dir: &Path) -> bool {
        dir.join("picus.toml").exists() && javascript_source_layout(dir).is_some()
    }

    fn build(&self, dir: &Path, _release: bool, shared_target: &Path) -> Result<BuildOutput> {
        self.build_with_runtime_class(dir, _release, shared_target, None)
    }

    fn build_with_runtime_class(
        &self,
        dir: &Path,
        _release: bool,
        shared_target: &Path,
        runtime_class: Option<&str>,
    ) -> Result<BuildOutput> {
        let runtime_class = runtime_class.unwrap_or(BundleKind::JsBundleV1.runtime_class());
        let function_id = read_function_id(dir)
            .context("JavaScript bundle dir must have picus.toml with [function].id")?;
        javascript_source_layout(dir).ok_or_else(|| {
            anyhow!(
                "JavaScript function {} is missing src/index.mjs",
                dir.display()
            )
        })?;

        let artifact_dir = shared_target.join("js-artifacts");
        std::fs::create_dir_all(&artifact_dir)
            .with_context(|| format!("Failed to create {}", artifact_dir.display()))?;
        let artifact = artifact_dir.join(format!("{}.tar.gz", function_id));
        let allowed_capabilities = PicusConfig::load_from_dir(dir)?.allowed_capabilities()?;

        bundler::package_bundle_with_capabilities_for_runtime_class(
            BundleKind::JsBundleV1,
            dir,
            &artifact,
            &allowed_capabilities,
            runtime_class,
        )
        .with_context(|| {
            format!(
                "Failed to package javascript-v1 bundle from {}",
                dir.display()
            )
        })?;

        Ok(BuildOutput { artifact })
    }
}

/// `path = "..."` dependencies declared in a `Cargo.toml` table, as `(crate name, declared path)`.
fn path_dependencies(manifest: &toml::Value) -> Vec<(String, String)> {
    fn from_table(table: &toml::Value, out: &mut Vec<(String, String)>) {
        let Some(table) = table.as_table() else { return };
        for (name, dep) in table {
            if let Some(path) = dep.get("path").and_then(|p| p.as_str()) {
                out.push((name.clone(), path.to_string()));
            }
        }
    }
    let mut out = Vec::new();
    for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(table) = manifest.get(key) {
            from_table(table, &mut out);
        }
    }
    if let Some(targets) = manifest.get("target").and_then(|t| t.as_table()) {
        for target in targets.values() {
            for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
                if let Some(table) = target.get(key) {
                    from_table(table, &mut out);
                }
            }
        }
    }
    if let Some(table) = manifest.get("workspace").and_then(|w| w.get("dependencies")) {
        from_table(table, &mut out);
    }
    // `[patch.<registry>]` entries may point at a local checkout too.
    if let Some(patches) = manifest.get("patch").and_then(|p| p.as_table()) {
        for registry in patches.values() {
            from_table(registry, &mut out);
        }
    }
    out
}

/// `a/b/../c` as `a/c`, without touching the filesystem (the path may not exist).
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

fn read_manifest(dir: &Path) -> Result<toml::Value> {
    let path = dir.join("Cargo.toml");
    let text = std::fs::read_to_string(&path).with_context(|| format!("Failed to read {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("Invalid TOML in {}", path.display()))
}

/// The directory to mount into the Linux build container for a function: the closest common
/// ancestor of the function, every path dependency it reaches (transitively), and the cargo
/// workspace above it if it is a member of one.
///
/// A path dependency that does not exist is an error naming the dependency, the path declared and
/// the Cargo.toml that declares it, since Cargo itself only reports the failure inside the
/// container, where the host paths are not visible.
fn docker_mount_root(abs_dir: &Path) -> Result<PathBuf> {
    let mut roots: Vec<PathBuf> = vec![abs_dir.to_path_buf()];
    let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let mut queue: Vec<PathBuf> = vec![abs_dir.to_path_buf()];
    let mut missing: Vec<String> = Vec::new();

    while let Some(dir) = queue.pop() {
        if !seen.insert(dir.clone()) {
            continue;
        }
        let manifest = read_manifest(&dir)?;
        for (name, declared) in path_dependencies(&manifest) {
            let target = dir.join(&declared);
            match target.canonicalize() {
                Ok(resolved) if resolved.join("Cargo.toml").exists() => {
                    roots.push(resolved.clone());
                    queue.push(resolved);
                }
                _ => missing.push(format!("path dependency `{name}` = \"{declared}\" not found (expected {})", lexical_normalize(&target).display())),
            }
        }
    }
    if !missing.is_empty() {
        // No function name or manifest path here: the same message for every function that shares a
        // missing dependency lets a batch deploy report them together.
        bail!("{}", missing.join("; "));
    }

    // Cargo finds a parent workspace by walking up from the function; the container must see it too.
    if read_manifest(abs_dir).ok().is_some_and(|m| m.get("workspace").is_none()) {
        for ancestor in abs_dir.ancestors().skip(1) {
            if read_manifest(ancestor).ok().is_some_and(|m| m.get("workspace").is_some()) {
                roots.push(ancestor.to_path_buf());
                break;
            }
        }
    }

    let mut common = roots[0].clone();
    for root in &roots[1..] {
        while !root.starts_with(&common) {
            if !common.pop() {
                break;
            }
        }
    }
    if common.parent().is_none() {
        bail!(
            "The path dependencies of {} only share the filesystem root, so mounting them would expose the whole disk. Keep them under one directory.",
            abs_dir.display()
        );
    }
    Ok(common)
}

fn read_function_id(dir: &Path) -> Result<String> {
    let path = dir.join("picus.toml");
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read {}", path.display()))?;
    let parsed: toml::Value = toml::from_str(&content)?;
    parsed
        .get("function")
        .and_then(|f| f.get("id"))
        .and_then(|n| n.as_str())
        .map(|s| s.to_string())
        .context("Could not find [function].id in picus.toml")
}

// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Python adapter — CPython source-bundle.
//
// Authoring contract: a Python function dir contains `picus.toml`,
// `src/handler.py`, optionally `pyproject.toml`, and `openapi.yaml`. The
// adapter stages `src/handler.py` into the `python-v1` source-bundle layout
// under `<shared_target>/python-bundles/<id>/` (`payload/handler.py`) and then
// invokes [`bundler::package_bundle`] to validate and produce a
// deterministic `.tar.gz` artifact. The runtime class for this lane is
// `python-v1`, executed by the `picus-py-shim` Guest Shim (CPython 3.12,
// stdlib only) inside a gVisor Pod cold-started by `compute-dispatcher`.
//
// This adapter compiles no wasm.
//
// Source-of-truth contract:
//   docs-platform/01-architecture/04-compute-architecture.md
// ---------------------------------------------------------------------------

pub struct PythonAdapter;

impl LanguageAdapter for PythonAdapter {
    fn language(&self) -> Language {
        Language::Python
    }

    fn detect(&self, dir: &Path) -> bool {
        // Python-only authoring layout: picus.toml + src/handler.py.
        // We do NOT require Cargo.toml; presence of Cargo.toml in a function
        // dir is a migration debt signal, not a marker for the Python lane.
        dir.join("picus.toml").exists() && python_source_layout(dir).is_some()
    }

    fn build(&self, dir: &Path, _release: bool, shared_target: &Path) -> Result<BuildOutput> {
        self.build_with_runtime_class(dir, _release, shared_target, None)
    }

    fn build_with_runtime_class(
        &self,
        dir: &Path,
        _release: bool,
        shared_target: &Path,
        runtime_class: Option<&str>,
    ) -> Result<BuildOutput> {
        let runtime_class = runtime_class.unwrap_or(BundleKind::PySourceBundleV1.runtime_class());
        let function_id = read_function_id(dir)
            .context("Python function dir must have picus.toml with [function].id")?;
        python_source_layout(dir).ok_or_else(|| {
            anyhow!(
                "Python function {} is missing src/handler.py",
                dir.display()
            )
        })?;
        let out_dir = shared_target.join("python-artifacts");
        std::fs::create_dir_all(&out_dir)
            .with_context(|| format!("Failed to create artifact dir {}", out_dir.display()))?;
        let archive_path = out_dir.join(format!("{}.tar.gz", function_id));
        let allowed_capabilities = PicusConfig::load_from_dir(dir)?.allowed_capabilities()?;
        let artifact = crate::bundler::package_bundle_with_capabilities_for_runtime_class(
            crate::bundler::BundleKind::PySourceBundleV1,
            dir,
            &archive_path,
            &allowed_capabilities,
            runtime_class,
        )?;

        Ok(BuildOutput {
            artifact: artifact.archive_path,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::detect_language;
    use std::fs;
    use std::path::Path;

    fn write(file: &Path, content: &str) {
        if let Some(parent) = file.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(file, content).unwrap();
    }

    #[test]
    fn detect_language_rejects_legacy_javascript_payload_layout() {
        let dir = std::env::temp_dir().join("picus-language-js-payload-legacy");
        let _ = fs::remove_dir_all(&dir);
        write(
            &dir.join("picus.toml"),
            "[function]\nid = \"legacy-js\"\nlanguage = \"javascript\"\n",
        );
        write(
            &dir.join("payload/index.mjs"),
            "export default function handler() {}\n",
        );

        let err = detect_language(&dir).unwrap_err();
        assert!(format!("{}", err).contains("src/index.mjs"));
    }

    #[test]
    fn detect_language_rejects_legacy_python_root_layout() {
        let dir = std::env::temp_dir().join("picus-language-py-root-legacy");
        let _ = fs::remove_dir_all(&dir);
        write(
            &dir.join("picus.toml"),
            "[function]\nid = \"legacy-py-root\"\nlanguage = \"python\"\n",
        );
        write(
            &dir.join("handler.py"),
            "def handle(request, context):\n    return {}\n",
        );

        let err = detect_language(&dir).unwrap_err();
        assert!(format!("{}", err).contains("src/handler.py"));
    }

    #[test]
    fn detect_language_rejects_legacy_python_payload_layout() {
        let dir = std::env::temp_dir().join("picus-language-py-payload-legacy");
        let _ = fs::remove_dir_all(&dir);
        write(
            &dir.join("picus.toml"),
            "[function]\nid = \"legacy-py-payload\"\nlanguage = \"python\"\n",
        );
        write(
            &dir.join("payload/handler.py"),
            "def handle(request, context):\n    return {}\n",
        );

        let err = detect_language(&dir).unwrap_err();
        assert!(format!("{}", err).contains("src/handler.py"));
    }
}
