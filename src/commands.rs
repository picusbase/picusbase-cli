use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "picusbase")]
#[command(about = "PicusBase CLI")]
#[command(version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    /// Named environment profile override loaded from the active CLI config
    #[arg(long, global = true)]
    pub profile: Option<String>,

    /// API URL override for the Compute Control Plane service
    #[arg(long, global = true)]
    pub url: Option<String>,

    /// Functions endpoint URL for `function run` (default: --url with `console.` → `functions.`)
    #[arg(long, global = true)]
    pub executor_url: Option<String>,

    /// Project Reference ID override (e.g. p_12345678)
    #[arg(long, global = true)]
    pub project_ref: Option<String>,

    /// Compute-audience bearer token for `function run` (skips the PAT exchange)
    #[arg(long, global = true)]
    pub auth_token: Option<String>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Configure CLI defaults
    Config(ConfigArgs),
    /// Manage Compute Functions
    #[command(alias = "functions")]
    Function(FunctionArgs),
    /// Manage Background Jobs
    #[command(alias = "jobs")]
    Job(JobArgs),

    /// Discover and call the console-api operations documented in the API spec
    Api(ApiArgs),
    /// Manage HTTP route bindings of Compute Functions
    #[command(alias = "routes")]
    Route(RouteArgs),
    /// Manage Schedules
    #[command(alias = "schedules")]
    Schedule(ScheduleArgs),

    /// Deploy resources
    Deploy(DeployArgs),

    /// Initialize resources
    Init(InitArgs),

    /// Phase 4 slice B/C: package a JS or Python source folder into the
    /// publishable bundle artifact (tar.gz) for the polyglot lanes.
    Bundle(BundleArgs),

    /// Local kind cluster development operations
    Dev(DevArgs),

    /// 3-step deploy flow: prepare Kubernetes, bootstrap kube-manager, then
    /// have kube-manager install the full picusbase stack via Helm — same
    /// commands for local dev and product (see PLAN-cluster-manager-deploy.md)
    Cluster(ClusterArgs),

    /// Deploy and manage consumer apps on PicusBase infrastructure
    #[command(alias = "apps")]
    App(AppArgs),
    /// Custom domains of project Apps (Console → Builder → Apps)
    Domain(DomainArgs),
    /// Generate code from the bundle (TypeScript types for frontends)
    Gen(GenArgs),
    /// Entity record data (restore from SQL dumps)
    Data(DataArgs),

    /// Manage PicusBase platform resources (organizations, projects)
    Platform(PlatformArgs),

    /// Check for and install the latest CLI version
    Upgrade,
}

// ── Platform commands ─────────────────────────────────────────────────────────

#[derive(Parser)]
pub struct PlatformArgs {
    #[command(subcommand)]
    pub command: PlatformCommands,
}

#[derive(Subcommand)]
pub enum PlatformCommands {
    /// Create an organization; the caller (the PAT's owner) becomes its owner
    OrgCreate {
        /// Organization display name
        #[arg(long)]
        name: String,
    },

    /// Create a project under an organization (the caller needs owner or admin there)
    ProjectCreate {
        /// Organization ID (UUID)
        #[arg(long)]
        org_id: String,

        /// Project display name
        #[arg(long)]
        name: String,
    },

    /// List organizations (uses PICUS_PERSONAL_TOKEN from config)
    OrgList {
        /// Console API base URL (defaults to configured --default-url)
        #[arg(long)]
        console_url: Option<String>,
    },

    /// List projects, optionally filtered by org (uses PICUS_PERSONAL_TOKEN from config)
    ProjectList {
        /// Filter by organization ID
        #[arg(long)]
        org_id: Option<String>,

        /// Console API base URL (defaults to configured --default-url)
        #[arg(long)]
        console_url: Option<String>,
    },

    /// Show project provisioning status
    ProjectStatus {
        /// Project reference (e.g. p_5acd949e)
        #[arg(long)]
        project_ref: String,
    },
}

// ── App commands ──────────────────────────────────────────────────────────────

#[derive(Parser)]
pub struct AppArgs {
    #[command(subcommand)]
    pub command: AppCommands,
}

#[derive(Subcommand)]
pub enum AppCommands {
    /// Remove an app entry from the Helm values file and reconcile the cluster
    Remove {
        /// Path to app.toml
        #[arg(long, default_value = "app.toml")]
        config: PathBuf,

        /// Target environment: local or product
        #[arg(long, default_value = "local")]
        env: String,
    },

    /// Build the app Docker image
    Build {
        /// Path to app.toml
        #[arg(long, default_value = "app.toml")]
        config: PathBuf,

        /// Image tag to build
        #[arg(long)]
        tag: String,

        /// Target environment: local or product
        #[arg(long, default_value = "local")]
        env: String,

        /// Extra --build-arg KEY=VALUE pairs (repeat for multiple)
        #[arg(long = "build-arg")]
        build_args: Vec<String>,

        /// Force clean build (drops dx/wasm/release caches)
        #[arg(long, default_value_t = false)]
        force_clean: bool,
    },

    /// Push the app Docker image to the registry
    Push {
        /// Path to app.toml
        #[arg(long, default_value = "app.toml")]
        config: PathBuf,

        /// Image tag to push
        #[arg(long)]
        tag: String,

        /// Target environment: local or product
        #[arg(long, default_value = "local")]
        env: String,
    },

    /// Deploy the app to Kubernetes via Helm
    Deploy {
        /// Path to app.toml
        #[arg(long, default_value = "app.toml")]
        config: PathBuf,

        /// Image tag to deploy
        #[arg(long)]
        tag: String,

        /// Target environment: local or product
        #[arg(long, default_value = "local")]
        env: String,

        /// Admin project ref (required when --migrate is used)
        #[arg(long)]
        project_ref: Option<String>,

        /// Run database migrations before deploying
        #[arg(long, default_value_t = false)]
        migrate: bool,

        /// Extra --build-arg KEY=VALUE pairs forwarded to docker build
        #[arg(long = "build-arg")]
        build_args: Vec<String>,
    },

    /// Run database migrations for this app against a PicusBase project
    Migrate {
        /// Path to app.toml
        #[arg(long, default_value = "app.toml")]
        config: PathBuf,

        /// Target project ref (e.g. p_f83354e4)
        #[arg(long)]
        project_ref: String,

        /// Target environment: local or product
        #[arg(long, default_value = "local")]
        env: String,

        /// Only run specific migration files (comma-separated filenames)
        #[arg(long)]
        only: Option<String>,
    },

    /// Show status of the deployed app
    Status {
        /// App ID (from app.toml [app].id)
        #[arg(long)]
        app_id: String,

        /// Target environment: local or product
        #[arg(long, default_value = "local")]
        env: String,
    },

    /// List all deployed apps
    List {
        /// Target environment: local or product
        #[arg(long, default_value = "local")]
        env: String,
    },

    /// Rollback the app to a previous Helm revision
    Rollback {
        /// Target environment: local or product
        #[arg(long, default_value = "local")]
        env: String,

        /// Helm revision number (defaults to previous revision)
        #[arg(long)]
        revision: Option<u32>,
    },

    /// Full orchestration: migrate → functions → build → deploy
    Start {
        /// Path to app.toml
        #[arg(long, default_value = "app.toml")]
        config: PathBuf,

        /// Admin project ref
        #[arg(long)]
        project_ref: String,

        /// Image tag to build and deploy
        #[arg(long)]
        tag: String,

        /// Target environment: local or product
        #[arg(long, default_value = "local")]
        env: String,

        /// Skip the migration step
        #[arg(long, default_value_t = false)]
        skip_migrate: bool,

        /// Skip the functions deploy step
        #[arg(long, default_value_t = false)]
        skip_functions: bool,

        /// Skip the functions sync step
        #[arg(long, default_value_t = false)]
        skip_sync: bool,

        /// Skip the image build step (use existing image)
        #[arg(long, default_value_t = false)]
        skip_build: bool,

        /// Only deploy functions, skip all other steps
        #[arg(long, default_value_t = false)]
        only_functions: bool,

        /// Extra --build-arg KEY=VALUE pairs for docker build
        #[arg(long = "build-arg")]
        build_args: Vec<String>,

        /// Force clean build
        #[arg(long, default_value_t = false)]
        force_clean: bool,
    },

    /// Manage app compute functions
    Functions(AppFunctionsArgs),
}

#[derive(Parser)]
pub struct AppFunctionsArgs {
    #[command(subcommand)]
    pub command: AppFunctionsCommands,
}

#[derive(Subcommand)]
pub enum AppFunctionsCommands {
    /// Deploy compute functions for this app
    Deploy {
        /// Path to app.toml
        #[arg(long, default_value = "app.toml")]
        config: PathBuf,

        /// Admin project ref
        #[arg(long)]
        project_ref: String,

        /// Target environment: local or product
        #[arg(long, default_value = "local")]
        env: String,

        /// Deploy only a specific function by key
        #[arg(long)]
        function: Option<String>,
    },

    /// Sync functions to all active tenants
    Sync {
        /// Path to app.toml
        #[arg(long, default_value = "app.toml")]
        config: PathBuf,

        /// Admin project ref
        #[arg(long)]
        project_ref: String,

        /// Target environment: local or product
        #[arg(long, default_value = "local")]
        env: String,
    },
}

#[derive(Parser)]
pub struct ClusterArgs {
    #[command(subcommand)]
    pub command: ClusterCommands,
}

#[derive(Subcommand)]
pub enum ClusterCommands {
    /// Step 1 (local dev only): create the kind cluster + Traefik +
    /// metrics-server — nothing app-level. Product's Step 1 is the separate
    /// SSH/K3s bootstrap runbook (VM-level, not something this CLI can do
    /// from a laptop) — see docs-picusbase/05-operations/product/01-bootstrap/.
    PrepareLocal {
        /// Kind cluster name (default: picusbase-local)
        #[arg(long)]
        cluster: Option<String>,
    },

    /// Local dev only: load already-built images (`picusbase dev build`)
    /// into the kind cluster's node — kind runs its own containerd, images
    /// in the local Docker daemon aren't visible to it otherwise.
    LoadImagesLocal {
        /// Kind cluster name (default: picusbase-local)
        #[arg(long)]
        cluster: Option<String>,
    },

    /// `helm install/upgrade` the standalone infra chart
    /// (infrastructure/helm/kube-manager/ — a separate chart directory, not a
    /// values overlay on the app chart) on an already-prepared cluster (kind
    /// for dev, k3s for product) — see PLAN-cluster-manager-deploy.md Phase M.
    /// This release is infrastructure, not an app instance: it stays up for
    /// the lifetime of the cluster and is never named the same as (or
    /// touched by installing/uninstalling) any picusbase app instance created
    /// via `cluster install`/`cluster upgrade`. The chart's own values.yaml
    /// already has local-dev-ready defaults (`runtimeSecret.create: true`
    /// with real credentials baked in) — no extra values file needed for
    /// local dev.
    Bootstrap {
        /// Kubernetes namespace for the standalone infra release (default:
        /// picusbase-kube-manager — deliberately NOT "picusbase", which is
        /// reserved for app instances)
        #[arg(long, default_value = "picusbase-kube-manager")]
        namespace: String,

        /// Runtime secret name to create/patch the bootstrap token onto —
        /// only used when --extra-values points at a values file with
        /// `runtimeSecret.create: false` (product-style operator-managed
        /// secret). Default: picusbase-bootstrap.
        #[arg(long)]
        secret_name: Option<String>,

        /// Helm release name for the standalone infra release (default:
        /// kube-manager — deliberately NOT "picusbase", which is reserved for
        /// app instances; see the doc comment above)
        #[arg(long, default_value = "kube-manager")]
        release: String,

        /// Extra values file layered on top of the chart's own values.yaml —
        /// leave unset for local dev (the chart's defaults already work).
        /// For product, pass a values file with `runtimeSecret.create:
        /// false`; the bootstrap token/admin credentials are then created
        /// out-of-band first via `generate-kube-manager-bootstrap-token.sh`
        /// (see `--secret-name` above).
        #[arg(long)]
        extra_values: Option<String>,
    },

    /// Step 3: call kube-manager's `POST /cluster/install` to bring up a
    /// picusbase app instance (data plane + every app-layer service, NOT
    /// kube-manager/kube-manager-ui — those stay on the separate release
    /// Bootstrap created) on its own release/namespace. Repeatable with a
    /// different `--namespace`/`--release` to run more than one instance on
    /// the same cluster.
    Install {
        /// Path to the Helm values file (e.g.
        /// infrastructure/helm/picusbase/values-local.yaml), relative to the
        /// repo root or absolute
        #[arg(long)]
        values_file: String,

        #[arg(long, default_value = "picusbase")]
        namespace: String,

        #[arg(long, default_value = "picusbase")]
        release: String,

        /// kube-manager base URL (default: http://localhost:4090 — matches
        /// `kubectl port-forward svc/<kube-manager-release>-kube-manager 4090:4090`)
        #[arg(long)]
        kube_manager_url: Option<String>,

        /// Bootstrap token from Step 1 (or set KUBE_MANAGER_BOOTSTRAP_TOKEN)
        #[arg(long)]
        token: Option<String>,

        /// Skip Vitess/RustFS data-plane provisioning (already provisioned)
        #[arg(long, default_value_t = false)]
        skip_data_plane: bool,
    },

    /// Same as Install, but for an existing release after a code-only
    /// change — always skips data-plane provisioning.
    Upgrade {
        #[arg(long)]
        values_file: String,

        #[arg(long, default_value = "picusbase")]
        namespace: String,

        #[arg(long, default_value = "picusbase")]
        release: String,

        #[arg(long)]
        kube_manager_url: Option<String>,

        #[arg(long)]
        token: Option<String>,
    },
}

#[derive(Parser)]
pub struct DevArgs {
    #[command(subcommand)]
    pub command: DevCommands,
}

#[derive(Subcommand)]
pub enum DevCommands {
    /// Set up or reset the local kind cluster and deploy all platform services
    Setup {
        /// Delete and recreate the kind cluster before setup
        #[arg(long, default_value_t = false)]
        reset: bool,

        /// Skip building Docker images
        #[arg(long, default_value_t = false)]
        no_build: bool,

        /// Skip loading images into kind
        #[arg(long, default_value_t = false)]
        no_load: bool,

        /// Kind cluster name (default: picusbase-local)
        #[arg(long)]
        cluster: Option<String>,

        /// Also deploy consumer apps from app.toml files discovered in the repo
        #[arg(long, default_value_t = false)]
        with_apps: bool,

        /// App tag to use when deploying apps with --with-apps (default: local)
        #[arg(long, default_value = "local")]
        app_tag: String,
    },

    /// Rebuild and hot-reload a single service in the running kind cluster
    Reload {
        /// Name of the service to reload (e.g. auth-service)
        service: String,

        /// Kind cluster name (default: picusbase-local)
        #[arg(long)]
        cluster: Option<String>,
    },

    /// Build Docker images for platform services
    Build {
        /// Build only specified service(s). Repeat to build multiple.
        #[arg(long)]
        service: Vec<String>,

        /// Image tag
        #[arg(long)]
        tag: Option<String>,

        /// Target platform (e.g. linux/arm64, linux/amd64)
        #[arg(long)]
        platform: Option<String>,
    },

    /// Push Docker images to the registry
    Push {
        /// Push only specified service(s). Repeat to push multiple.
        #[arg(long)]
        service: Vec<String>,

        /// Image tag
        #[arg(long)]
        tag: Option<String>,
    },

    /// Apply runtime Kubernetes secrets from an env file
    ApplySecret {
        /// Path to the env file containing secret values
        #[arg(long)]
        env_file: Option<PathBuf>,

        /// Kubernetes namespace (default: picusbase)
        #[arg(long)]
        namespace: Option<String>,
    },

    /// Port-forward local kind cluster functions endpoint to localhost
    PortForward,
}

#[derive(Parser)]
pub struct GenArgs {
    #[command(subcommand)]
    pub command: GenCommands,
}

#[derive(Subcommand)]
pub enum GenCommands {
    /// TypeScript types + entity metadata from doctypes/ (for frontends)
    Types {
        /// Bundle directory (contains doctypes/)
        #[arg(long, default_value = ".")]
        path: PathBuf,
        /// Output .ts file
        #[arg(long, default_value = "frontends/shared/src/generated/entities.ts")]
        out: PathBuf,
    },
}

#[derive(Parser)]
pub struct DataArgs {
    #[command(subcommand)]
    pub command: DataCommands,
}

#[derive(Subcommand)]
pub enum DataCommands {
    /// Replace entity records from <dir>/*.sql dumps
    Restore {
        /// Directory with <entity>.sql files
        #[arg(long, default_value = "data")]
        dir: PathBuf,
    },
}

#[derive(Parser)]
pub struct BundleArgs {
    #[command(subcommand)]
    pub command: BundleCommands,
}

#[derive(Subcommand)]
pub enum BundleCommands {
    /// Validate and package a `js-bundle-v1` function source folder.
    /// Authoring layout must contain `src/index.mjs`.
    Js {
        /// Source folder to package.
        #[arg(long, default_value = ".")]
        src: PathBuf,
        /// Output `.tar.gz` file path.
        #[arg(long)]
        out: PathBuf,
    },
    /// Validate and package a `py-source-bundle-v1` function source folder.
    /// Authoring layout must contain `src/handler.py` (stdlib only).
    Python {
        /// Source folder to package.
        #[arg(long, default_value = ".")]
        src: PathBuf,
        /// Output `.tar.gz` file path.
        #[arg(long)]
        out: PathBuf,
        /// Runtime class to embed in manifest.json.
        #[arg(long, default_value = "python-v1")]
        runtime_class: String,
    },
}

#[derive(Parser)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub command: ConfigCommands,
}

#[derive(Subcommand)]
pub enum ConfigCommands {
    /// Show configured root defaults, named profiles, and current effective values
    Show,
    /// Persist CLI values to root defaults or to the selected named profile
    Set {
        /// Default API URL written to the CLI config file
        #[arg(long)]
        default_url: Option<String>,

        /// Default functions endpoint URL written to the CLI config file
        #[arg(long)]
        default_executor_url: Option<String>,

        /// Default project ref written to the CLI config file
        #[arg(long)]
        default_project_ref: Option<String>,

        /// Personal access token (pbpat_...) from Console → System Admin → Access Tokens.
        /// When set, --default-url must be the Console URL (e.g. http://console.picusbase.localhost).
        #[arg(long)]
        personal_token: Option<String>,
    },
    /// Set or clear the default profile used when --profile is not provided
    Use {
        /// Profile name to make the default for future CLI invocations
        profile: Option<String>,

        /// Clear the default profile and fall back to stored root defaults
        #[arg(long, default_value_t = false)]
        clear: bool,
    },
    /// Delete a named profile from the CLI config file
    DeleteProfile {
        /// Profile name to delete
        profile: String,
    },
    /// Move current root defaults into a named profile, set it as default, and clear the root keys
    MigrateRootDefaults {
        /// Target profile name that will receive the current root defaults
        profile: String,
    },
}

#[derive(Parser)]
pub struct InitArgs {
    #[command(subcommand)]
    pub command: InitCommands,
}

#[derive(Subcommand)]
pub enum InitCommands {
    /// Initialize a new HTTP function scaffold under the current directory
    Functions {
        /// Logical function domain written to picus.toml (e.g. inventory)
        domain: Option<String>,

        /// Function ID (slug, e.g. inventory-suppliers)
        function_id: Option<String>,

        /// Friendly display name
        #[arg(long)]
        name: Option<String>,

        /// Function ID (slug) as a flag, kept for backward compatibility
        #[arg(long)]
        id: Option<String>,

        /// Project reference written to picus.toml
        #[arg(long)]
        project_ref: Option<String>,

        /// Skip updating the nearest managed functions workspace when scaffolding inside that tree
        #[arg(long, default_value_t = false)]
        no_workspace_member: bool,

        /// Guest language for the new function. Supported scaffolds today are
        /// `rust`, `javascript`, and `python`. Non-Rust lanes scaffold their
        /// source under `src/` and are normalized into the active bundle
        /// contracts at build time.
        #[arg(long, default_value = "rust")]
        language: String,
    },
    /// Scaffold a frontend app (React + Vite) wired to the project's entities and functions
    App {
        /// App code (lowercase letters, digits, `_`), e.g. demo_app
        code: String,
        /// Frontend framework (supported: react)
        #[arg(long, default_value = "react")]
        framework: String,
        /// Display name
        #[arg(long)]
        name: Option<String>,
        /// Domain the app is deployed to (default for local: <code>.picus.localhost)
        #[arg(long)]
        domain: Option<String>,
        /// Target directory (default: ./<code with - instead of _>)
        #[arg(long)]
        dir: Option<PathBuf>,
    },
}

#[derive(Parser)]
pub struct DeployArgs {
    #[command(subcommand)]
    pub command: DeployCommands,
}

#[derive(Subcommand)]
pub enum DeployCommands {
    /// Build and deploy functions from current directory
    Functions {
        /// Optional list of function IDs to deploy (e.g., `picusbase deploy functions func1 func2`)
        #[arg(help = "List of function IDs to deploy")]
        ids: Vec<String>,

        /// Path to the function directory (containing picus.toml) or parent directory
        #[arg(long, default_value = ".")]
        path: PathBuf,

        /// Force rebuild
        #[arg(long)]
        rebuild: bool,

        /// Phase 4 slice A: Override the runtime class advertised to the
        /// control plane (defaults to `wasm-core-v1` for the rust adapter).
        #[arg(long)]
        runtime_class: Option<String>,

        /// Phase 4 slice A: Override the manifest entrypoint (defaults to
        /// `main` for the rust adapter).
        #[arg(long)]
        entrypoint: Option<String>,

        /// Phase 4 slice A: Path to an Ed25519 signing key (PKCS8 PEM or raw
        /// 32-byte seed). When supplied alongside --signer-id, the CLI will
        /// sign the artifact checksum and include the signature in publish.
        #[arg(long)]
        signing_key_path: Option<PathBuf>,

        /// Phase 4 slice A: Signer identity registered in
        /// `system.compute_artifact_signers`. Required when
        /// --signing-key-path is provided.
        #[arg(long)]
        signer_id: Option<String>,
    },
    /// Deploy the data layer of a bundle: doctypes/, roles/, functions/, automations/, ai_agent/
    Schema {
        /// Bundle directory (contains doctypes/)
        #[arg(long, default_value = ".")]
        path: PathBuf,
        /// Skip building/deploying functions/
        #[arg(long, default_value_t = false)]
        skip_functions: bool,
        /// Draft entity versions without publishing them
        #[arg(long, default_value_t = false)]
        no_publish: bool,
        /// Owner (user id) of newly created AI agents (default: the logged-in user)
        #[arg(long)]
        bound_user_id: Option<String>,
        /// Check the bundle and show what would be created or updated, without writing anything
        #[arg(long, alias = "plan", default_value_t = false)]
        dry_run: bool,
        /// When a function fails, deploy the remaining ones and report every failure at the end
        #[arg(long, default_value_t = false)]
        keep_going: bool,
    },
    /// Build a frontend app (picus.app.toml) and deploy it to its domain
    App {
        /// App directory containing picus.app.toml
        #[arg(long, default_value = ".")]
        path: PathBuf,
        /// Override the domain from picus.app.toml
        #[arg(long)]
        domain: Option<String>,
        /// Deploy the existing build output without running the build command
        #[arg(long, default_value_t = false)]
        skip_build: bool,
    },
}

#[derive(Parser)]
pub struct FunctionArgs {
    #[command(subcommand)]
    pub command: FunctionCommands,
}

#[derive(Subcommand)]
pub enum FunctionCommands {
    /// List all functions
    List,
    /// Build a function artifact for the detected guest lane
    Build {
        /// Path to the function directory (default: current directory)
        #[arg(long, default_value = ".")]
        path: PathBuf,
        /// Build in release mode
        #[arg(long, default_value_t = true)]
        release: bool,
    },
    /// Upload a new function
    Upload {
        /// Unique ID for the function
        #[arg(long)]
        id: String,
        /// Friendly name for the function
        #[arg(long)]
        name: Option<String>,
        /// Path to the built artifact (.wasm or .tar.gz depending on lane)
        #[arg(long)]
        file: PathBuf,
        /// Phase 4 slice A overrides
        #[arg(long)]
        runtime_class: Option<String>,
        #[arg(long)]
        entrypoint: Option<String>,
        #[arg(long)]
        signing_key_path: Option<PathBuf>,
        #[arg(long)]
        signer_id: Option<String>,
        #[arg(long, default_value = "rust")]
        language: String,
    },
    /// Run a function immediately
    Run {
        /// Function ID to run
        #[arg(long)]
        id: String,
        /// JSON parameters (e.g. '{"key": "value"}')
        #[arg(long, default_value = "{}")]
        params: String,
    },
    /// Delete a function
    Delete {
        /// Function ID (picus.toml `id`) or registry UUID
        #[arg(long)]
        id: String,
    },
}

#[derive(Parser)]
pub struct ApiArgs {
    #[command(subcommand)]
    pub command: ApiCommands,
}

#[derive(Subcommand)]
pub enum ApiCommands {
    /// List the documented operations (start here: the methods and paths are in this list)
    List {
        /// Only this HTTP method: GET, POST, PUT, PATCH or DELETE
        #[arg(long)]
        method: Option<String>,
        /// Only operations whose id, path or summary contains this text
        #[arg(long)]
        filter: Option<String>,
        /// Print JSON instead of a table
        #[arg(long)]
        json: bool,
    },
    /// Show one operation: summary, parameters, request body, permissions, responses
    Describe {
        /// operationId, e.g. get_project_dashboard
        operation_id: String,
    },
    /// Call an operation with the profile's personal access token
    Call {
        /// HTTP method
        method: String,
        /// Path as listed by `picusbase api list`, e.g. /platform/projects/p_ab12cd34/dashboard
        path: String,
        /// Query parameter, repeatable: -q range=24h
        #[arg(long = "query", short = 'q')]
        query: Vec<String>,
        /// JSON request body
        #[arg(long)]
        data: Option<String>,
        /// Read the JSON request body from a file
        #[arg(long, conflicts_with = "data")]
        data_file: Option<PathBuf>,
        /// Send a request that is not GET (without this flag it is only printed)
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Parser)]
pub struct JobArgs {
    #[command(subcommand)]
    pub command: JobCommands,
}

#[derive(Subcommand)]
pub enum JobCommands {
    /// List recent jobs
    List,
    /// Create a new background job
    Create {
        /// Function ID to execute
        #[arg(long)]
        function_id: String,
        /// JSON parameters
        #[arg(long, default_value = "{}")]
        params: String,
    },
    /// Delete a job
    Delete {
        /// Job ID to delete
        #[arg(long)]
        id: String,
    },
}

#[derive(Parser)]
pub struct DomainArgs {
    #[command(subcommand)]
    pub command: DomainCommands,
}

#[derive(Subcommand)]
pub enum DomainCommands {
    /// List the project's Apps with their framework and domain
    List,
    /// Serve an App on a custom domain, or move it to a new one (no rebuild)
    Set {
        /// App code
        app_code: String,
        /// Full hostname, e.g. shop.example.com
        domain: String,
    },
    /// Take an App off its domain (undeploys its container)
    Remove {
        /// App code
        app_code: String,
    },
}

#[derive(Parser)]
pub struct RouteArgs {
    #[command(subcommand)]
    pub command: RouteCommands,
}

#[derive(Subcommand)]
pub enum RouteCommands {
    /// List route bindings of the project
    List,
    /// Bind an HTTP route to a function (upserts on path + method)
    Bind {
        /// Function ID (picus.toml `id`)
        #[arg(long)]
        function: String,
        /// Path pattern, e.g. /orders/{id}
        #[arg(long)]
        path: String,
        /// HTTP method
        #[arg(long, default_value = "POST")]
        method: String,
        /// none | authenticated | authorized
        #[arg(long, default_value = "authenticated")]
        auth_mode: String,
        /// Required permission key (repeatable), checked when auth_mode = authorized
        #[arg(long = "permission")]
        permissions: Vec<String>,
    },
    /// Delete a route binding
    Delete {
        /// Route binding ID (from `picusbase route list`)
        #[arg(long)]
        id: String,
    },
}

#[derive(Parser)]
pub struct ScheduleArgs {
    #[command(subcommand)]
    pub command: ScheduleCommands,
}

#[derive(Subcommand)]
pub enum ScheduleCommands {
    /// List all schedules
    List,
    /// Create a new schedule
    Create {
        /// Name of the schedule
        #[arg(long)]
        name: String,
        /// Cron expression (e.g. "0 * * * * *")
        #[arg(long)]
        cron: String,
        /// Function ID to execute
        #[arg(long)]
        function_id: String,
        /// JSON parameters
        #[arg(long, default_value = "{}")]
        params: String,
    },
    /// Delete a schedule
    Delete {
        /// Schedule ID to delete
        #[arg(long)]
        id: String,
    },
}

