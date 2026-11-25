use anyhow::Result;
use reqwest::blocking::{multipart, Client};
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Phase 4 slice A — optional fields the CLI can attach to a publish call.
/// Every field is `Option<String>`; the control plane (Phase 2 slice B/C)
/// treats all of them as nullable on the receiving side, so older CLIs
/// continue to work and newer CLIs can opt in field-by-field.
#[derive(Debug, Default, Clone)]
pub struct PublishExtras {
    pub language: Option<String>,
    pub runtime_class: Option<String>,
    pub entrypoint: Option<String>,
    pub manifest_checksum: Option<String>,
    pub allowed_capabilities: Option<String>,
    pub signature: Option<String>,
    pub signer_id: Option<String>,
    pub signature_algorithm: Option<String>,
    pub sbom_checksum: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, tabled::Tabled)]
pub struct Function {
    pub id: String,
    pub name: String,
    #[tabled(display_with = "display_option")]
    pub created_at: Option<String>,
    #[tabled(display_with = "display_option")]
    pub updated_at: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ComputeFunction {
    pub id: String,
    pub function_key: String,
    pub name: String,
    pub project_ref: String,
}

#[derive(Debug, Serialize, Deserialize, tabled::Tabled)]
pub struct RouteBinding {
    pub id: String,
    #[tabled(skip)]
    pub project_ref: String,
    pub path_pattern: String,
    pub http_method: String,
    pub function_id: String,
    #[tabled(skip)]
    pub version_id: Option<String>,
    #[tabled(skip)]
    pub policy_id: Option<String>,
    pub is_active: bool,
    #[serde(default)]
    pub auth_mode: String,
    #[serde(default)]
    #[tabled(display_with = "display_list")]
    pub required_permissions: Vec<String>,
    #[serde(default)]
    pub source: String,
}

#[derive(Serialize)]
struct CreateRouteBindingRequest {
    pub path_pattern: String,
    pub http_method: Option<String>,
    pub function_id: String,
    pub version_id: Option<String>,
    pub policy_id: Option<String>,
    pub auth_mode: Option<String>,
    pub required_permissions: Option<Vec<String>>,
    pub source: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, tabled::Tabled)]
pub struct Job {
    pub id: String,
    pub status: String,
    pub function_id: String,
    #[serde(default)]
    #[tabled(display_with = "display_option")]
    pub runtime_class: Option<String>,
    #[tabled(skip)]
    pub params: serde_json::Value,
    #[tabled(skip)]
    pub result: Option<serde_json::Value>,
    #[tabled(display_with = "display_option")]
    pub error_message: Option<String>,
    #[serde(default)]
    #[tabled(display_with = "display_option")]
    pub error_class: Option<String>,
    #[tabled(display_with = "display_option")]
    pub created_at: Option<String>,
    #[tabled(display_with = "display_option")]
    pub scheduled_at: Option<String>,
    #[tabled(display_with = "display_option")]
    pub started_at: Option<String>,
    #[tabled(display_with = "display_option")]
    pub completed_at: Option<String>,
    #[serde(default)]
    pub attempt_count: i32,
    #[serde(default)]
    pub max_retries: i32,
    #[serde(default)]
    pub retry_backoff_ms: i32,
}

#[derive(Debug, Serialize, Deserialize, tabled::Tabled)]
pub struct JobSchedule {
    pub id: String,
    pub name: String,
    pub cron_expression: String,
    pub function_id: String,
    #[tabled(skip)]
    pub default_params: serde_json::Value,
    pub is_active: bool,
    #[tabled(display_with = "display_option")]
    pub last_run_at: Option<String>,
    #[tabled(display_with = "display_option")]
    pub next_run_at: Option<String>,
}

fn display_option(o: &Option<String>) -> String {
    o.as_deref().unwrap_or("-").to_string()
}

fn display_list(items: &[String]) -> String {
    if items.is_empty() {
        "-".to_string()
    } else {
        items.join(", ")
    }
}

#[derive(Serialize)]
struct CreateJobRequest {
    function_id: String,
    wasm_module: String,
    params: serde_json::Value,
}

#[derive(Serialize)]
struct CreateScheduleRequest {
    name: String,
    cron_expression: String,
    function_id: String,
    default_params: serde_json::Value,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct OpenApiFragment {
    pub id: String,
    pub function_id: String,
    pub domain: String,
    pub fragment: serde_json::Value,
}

#[derive(Serialize)]
struct CreateOpenApiFragmentRequest {
    function_id: String,
    domain: String,
    fragment: serde_json::Value,
}

#[allow(dead_code)]
#[derive(Debug, Serialize, Deserialize)]
pub struct AggregatedSpec {
    pub domain: String,
    pub spec: serde_json::Value,
}

#[derive(Debug, Deserialize)]
pub struct PublishedVersion {
    pub id: String,
}

#[derive(Debug, Deserialize)]
pub struct PublishResult {
    pub function: Function,
    pub version: PublishedVersion,
}

pub struct PicusClient {
    client: Client,
    base_url: String,
    project_ref: Option<String>,
    executor_url: Option<String>,
    auth_token: Option<String>,
    personal_token: Option<String>,
    console_token: std::sync::Mutex<Option<(String, std::time::Instant)>>,
}

impl PicusClient {
    pub fn new(
        base_url: String,
        project_ref: Option<String>,
        executor_url: Option<String>,
        auth_token: Option<String>,
    ) -> Self {
        Self {
            client: Client::new(),
            base_url,
            project_ref,
            executor_url,
            auth_token,
            personal_token: None,
            console_token: std::sync::Mutex::new(None),
        }
    }

    pub fn with_personal_token(mut self, personal_token: Option<String>) -> Self {
        self.personal_token = personal_token;
        self
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Exchange the configured PAT for a short-lived bearer token.
    /// Errors if no PAT is configured.
    pub fn bearer_token(&self) -> Result<String> {
        let pat = self.personal_token.as_deref().ok_or_else(|| {
            anyhow::anyhow!(
                "personal_token is required: `picusbase config set --personal-token <pbpat_...>` or PICUSBASE_PERSONAL_TOKEN"
            )
        })?;
        self.console_token(pat)
    }

    /// With a personal access token `base_url` is the Console URL and requests go through
    /// console-api's authenticated `/platform/projects/:ref/compute` proxy; otherwise straight to
    /// the Compute Control Plane. `console_path: None` marks an endpoint console-api does not proxy.
    fn url(&self, cp_path: &str, console_path: Option<&str>) -> Result<String> {
        if self.personal_token.is_none() {
            return Ok(format!("{}/compute{}", self.base_url, cp_path));
        }
        let console_path = console_path.ok_or_else(|| {
            anyhow::anyhow!("This command is not available through console-api yet")
        })?;
        let project_ref = self.project_ref.as_deref().ok_or_else(|| {
            anyhow::anyhow!(
                "project_ref is required: --project-ref or PICUSBASE_PROJECT_REF"
            )
        })?;
        Ok(format!(
            "{}/platform/projects/{}/compute{}",
            self.base_url, project_ref, console_path
        ))
    }

    /// The exchanged access token, cached until a minute before it expires (about 15 minutes).
    fn console_token(&self, personal_token: &str) -> Result<String> {
        let mut cached = self.console_token.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((token, valid_until)) = cached.as_ref() {
            if std::time::Instant::now() < *valid_until {
                return Ok(token.clone());
            }
        }

        #[derive(Deserialize)]
        struct TokenResponse {
            access_token: String,
            expires_in: Option<u64>,
        }

        let res = self
            .client
            .post(format!("{}/api/auth/oauth/token", self.base_url))
            .json(&serde_json::json!({
                "grant_type": "personal_access_token",
                "personal_access_token": personal_token,
            }))
            .send()?;
        if !res.status().is_success() {
            anyhow::bail!(
                "Personal access token rejected by {} ({}): token revoked, expired, or wrong --url",
                self.base_url,
                res.status()
            );
        }
        let body = res.json::<TokenResponse>()?;
        let lifetime = body.expires_in.unwrap_or(300).saturating_sub(60).max(10);
        *cached = Some((
            body.access_token.clone(),
            std::time::Instant::now() + std::time::Duration::from_secs(lifetime),
        ));
        Ok(body.access_token)
    }

    fn request_builder(
        &self,
        method: reqwest::Method,
        url: &str,
    ) -> Result<reqwest::blocking::RequestBuilder> {
        let mut req = self.client.request(method, url);
        if let Some(ref p_ref) = self.project_ref {
            req = req.header("x-project-ref", p_ref);
        }
        if let Some(ref personal_token) = self.personal_token {
            req = req.bearer_auth(self.console_token(personal_token)?);
        }
        Ok(req)
    }

    /// Sends one request to a console-api path, for `picusbase api call`. Needs a PAT.
    pub fn send_api(
        &self,
        method: &str,
        path_and_query: &str,
        body: Option<&serde_json::Value>,
    ) -> Result<(u16, String)> {
        if self.personal_token.is_none() {
            anyhow::bail!(
                "personal_token is required: `picusbase config set --personal-token <pbpat_...>` or PICUSBASE_PERSONAL_TOKEN"
            );
        }
        let method = reqwest::Method::from_bytes(method.as_bytes())?;
        let url = format!("{}{}", self.base_url, path_and_query);
        let mut req = self.request_builder(method, &url)?;
        if let Some(body) = body {
            req = req.json(body);
        }
        let res = req.send()?;
        let status = res.status().as_u16();
        Ok((status, res.text()?))
    }

    // Functions
    pub fn list_functions(&self) -> Result<Vec<Function>> {
        let url = self.url("/registry/functions", Some("/functions"))?;
        let res = self.request_builder(reqwest::Method::GET, &url)?.send()?;

        if !res.status().is_success() {
            let text = res.text()?;
            anyhow::bail!("Failed to list functions: {}", text);
        }

        Ok(res.json()?)
    }

    #[allow(dead_code)]
    pub fn upload_function(&self, id: String, name: String, file_path: &Path) -> Result<Function> {
        self.upload_function_with_extras(id, name, file_path, &PublishExtras::default())
    }

    pub fn upload_function_publish_result_with_extras(
        &self,
        id: String,
        name: String,
        file_path: &Path,
        extras: &PublishExtras,
    ) -> Result<PublishResult> {
        let url = self.url("/registry/publish", Some("/functions"))?;

        let mut form = multipart::Form::new()
            .text("function_key", id)
            .text("name", name)
            .file("wasm_file", file_path)?;

        if let Some(ref v) = extras.runtime_class {
            form = form.text("runtime_class", v.clone());
        }
        if let Some(ref v) = extras.language {
            form = form.text("language", v.clone());
        }
        if let Some(ref v) = extras.entrypoint {
            form = form.text("entrypoint", v.clone());
        }
        if let Some(ref v) = extras.manifest_checksum {
            form = form.text("manifest_checksum", v.clone());
        }
        if let Some(ref v) = extras.allowed_capabilities {
            form = form.text("allowed_capabilities", v.clone());
        }
        if let Some(ref v) = extras.signature {
            form = form.text("signature", v.clone());
        }
        if let Some(ref v) = extras.signer_id {
            form = form.text("signer_id", v.clone());
        }
        if let Some(ref v) = extras.signature_algorithm {
            form = form.text("signature_algorithm", v.clone());
        }
        if let Some(ref v) = extras.sbom_checksum {
            form = form.text("sbom_checksum", v.clone());
        }

        let res = self
            .request_builder(reqwest::Method::POST, &url)?
            .multipart(form)
            .send()?;

        if !res.status().is_success() {
            let text = res.text()?;
            anyhow::bail!("Failed to upload function: {}", text);
        }

        Ok(res.json()?)
    }

    pub fn upload_function_with_extras(
        &self,
        id: String,
        name: String,
        file_path: &Path,
        extras: &PublishExtras,
    ) -> Result<Function> {
        Ok(self
            .upload_function_publish_result_with_extras(id, name, file_path, extras)?
            .function)
    }

    pub fn activate_version(&self, version_id: &str) -> Result<()> {
        let url = self.url(
            &format!("/registry/versions/{version_id}/activate"),
            Some(&format!("/versions/{version_id}/activate")),
        )?;
        let res = self.request_builder(reqwest::Method::POST, &url)?.send()?;

        if !res.status().is_success() {
            let text = res.text()?;
            anyhow::bail!("Failed to activate version: {}", text);
        }

        Ok(())
    }

    pub fn delete_function(&self, id: &str) -> Result<bool> {
        let url = self.url(&format!("/registry/functions/{id}"), Some(&format!("/functions/{id}")))?;
        let res = self.request_builder(reqwest::Method::DELETE, &url)?.send()?;

        if !res.status().is_success() {
            let text = res.text()?;
            anyhow::bail!("Failed to delete function: {}", text);
        }

        Ok(res.json()?)
    }

    /// `executor_url` if set, else the Console URL with its `console.` host label swapped for
    /// `functions.` (console.picusbase.localhost → functions.picusbase.localhost).
    pub fn functions_url(&self) -> Result<String> {
        if let Some(ref url) = self.executor_url {
            return Ok(url.trim_end_matches('/').to_string());
        }
        let base = self.base_url.trim_end_matches('/');
        let (scheme, rest) = base.split_once("://").unwrap_or(("https", base));
        let authority = rest.split('/').next().unwrap_or(rest);
        match authority.strip_prefix("console.") {
            Some(host) => Ok(format!("{scheme}://functions.{host}")),
            None => anyhow::bail!(
                "Cannot derive the functions URL from '{}': set --executor-url, PICUSBASE_EXECUTOR_URL or `picusbase config set --default-executor-url https://functions.<domain>`",
                self.base_url
            ),
        }
    }

    /// Dispatcher only accepts RS256 JWTs with audience `compute`, scoped to one project.
    fn compute_token(&self, project_ref: &str) -> Result<String> {
        #[derive(Deserialize)]
        struct TokenResponse {
            access_token: String,
        }

        let res = self
            .client
            .post(format!("{}/api/auth/token/exchange", self.base_url))
            .bearer_auth(self.bearer_token()?)
            .header("x-project-ref", project_ref)
            .json(&serde_json::json!({
                "target_project_ref": project_ref,
                "target_audience": "compute",
            }))
            .send()?;
        if !res.status().is_success() {
            let status = res.status();
            anyhow::bail!("Compute token exchange failed ({}): {}", status, res.text()?);
        }
        Ok(res.json::<TokenResponse>()?.access_token)
    }

    pub fn run_function(&self, id: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        let project_ref = self.project_ref.as_deref().ok_or_else(|| {
            anyhow::anyhow!(
                "project_ref is required: --project-ref or PICUSBASE_PROJECT_REF"
            )
        })?;
        let url = format!("{}/functions/{}/execute", self.functions_url()?, id);
        let token = match (&self.auth_token, &self.personal_token) {
            (Some(token), _) => Some(token.clone()),
            (None, Some(_)) => Some(self.compute_token(project_ref)?),
            (None, None) => None,
        };

        // Orchestrator functions (e.g. tenant provisioning) run for minutes — the client's default 30s would give up while they still finish.
        let mut req = self
            .client
            .post(&url)
            .timeout(std::time::Duration::from_secs(15 * 60))
            .header("x-project-ref", project_ref)
            .json(&params);
        if let Some(token) = token {
            req = req.bearer_auth(token);
        }

        let res = req.send()?;
        if !res.status().is_success() {
            let status = res.status();
            anyhow::bail!("Failed to run function ({}): {}", status, res.text()?);
        }

        Ok(res.json()?)
    }

    /// `None` if the app_code is not registered in the project.
    pub fn get_app(&self, project_ref: &str, app_code: &str) -> Result<Option<serde_json::Value>> {
        let res = self
            .client
            .get(format!("{}/platform/projects/{project_ref}/apps/{app_code}", self.base_url))
            .bearer_auth(self.bearer_token()?)
            .send()?;
        if res.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !res.status().is_success() {
            let status = res.status();
            anyhow::bail!("Failed to read app ({}): {}", status, res.text()?);
        }
        Ok(Some(res.json()?))
    }

    pub fn list_apps(&self, project_ref: &str) -> Result<Vec<serde_json::Value>> {
        let res = self
            .client
            .get(format!("{}/platform/projects/{project_ref}/apps", self.base_url))
            .bearer_auth(self.bearer_token()?)
            .send()?;
        if !res.status().is_success() {
            let status = res.status();
            anyhow::bail!("Failed to list apps ({}): {}", status, res.text()?);
        }
        Ok(res.json()?)
    }

    pub fn undeploy_app(&self, project_ref: &str, app_code: &str) -> Result<()> {
        let res = self
            .client
            .delete(format!("{}/platform/projects/{project_ref}/apps/{app_code}/deploy", self.base_url))
            .bearer_auth(self.bearer_token()?)
            .send()?;
        if !res.status().is_success() {
            let status = res.status();
            anyhow::bail!("Failed to undeploy app ({}): {}", status, res.text()?);
        }
        Ok(())
    }

    /// Registers (or updates) the app in the project, same endpoint as App Studio's "+ New App".
    pub fn define_app(&self, project_ref: &str, app_code: &str, app_name: &str, ui_framework: &str) -> Result<()> {
        let res = self
            .client
            .post(format!("{}/platform/projects/{project_ref}/apps", self.base_url))
            .bearer_auth(self.bearer_token()?)
            .json(&serde_json::json!({ "app_code": app_code, "app_name": app_name, "ui_framework": ui_framework }))
            .send()?;
        if !res.status().is_success() {
            let status = res.status();
            anyhow::bail!("Failed to register app ({}): {}", status, res.text()?);
        }
        Ok(())
    }

    /// Returns the bundle checksum to pass to `deploy_app`.
    pub fn upload_app_bundle(&self, project_ref: &str, app_code: &str, bundle: Vec<u8>) -> Result<String> {
        let res = self
            .client
            .put(format!("{}/platform/projects/{project_ref}/apps/{app_code}/bundle", self.base_url))
            .bearer_auth(self.bearer_token()?)
            .header("content-type", "application/gzip")
            .body(bundle)
            .send()?;
        if !res.status().is_success() {
            let status = res.status();
            anyhow::bail!("Failed to upload app bundle ({}): {}", status, res.text()?);
        }
        let body: serde_json::Value = res.json()?;
        body.get("checksum")
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("upload response has no checksum: {body}"))
    }

    /// `bundle_checksum: None` = server uses the app's latest bundle (react) or its pages (dioxus).
    pub fn deploy_app(&self, project_ref: &str, app_code: &str, domain: &str, bundle_checksum: Option<&str>) -> Result<serde_json::Value> {
        let res = self
            .client
            .post(format!("{}/platform/projects/{project_ref}/apps/{app_code}/deploy", self.base_url))
            .bearer_auth(self.bearer_token()?)
            .json(&serde_json::json!({ "domain": domain, "bundle_checksum": bundle_checksum }))
            .send()?;
        if !res.status().is_success() {
            let status = res.status();
            anyhow::bail!("Failed to deploy app ({}): {}", status, res.text()?);
        }
        Ok(res.json()?)
    }

    // Jobs
    pub fn list_jobs(&self) -> Result<Vec<Job>> {
        let url = self.url("/jobs", Some("/jobs"))?;
        let res = self.request_builder(reqwest::Method::GET, &url)?.send()?;

        if !res.status().is_success() {
            let text = res.text()?;
            anyhow::bail!("Failed to list jobs: {}", text);
        }

        Ok(res.json()?)
    }

    pub fn create_job(&self, function_id: String, params: serde_json::Value) -> Result<Job> {
        let url = self.url("/jobs", Some("/jobs"))?;
        let payload = CreateJobRequest {
            function_id: function_id.clone(),
            wasm_module: function_id,
            params,
        };

        let res = self
            .request_builder(reqwest::Method::POST, &url)?
            .json(&payload)
            .send()?;

        if !res.status().is_success() {
            let text = res.text()?;
            anyhow::bail!("Failed to create job: {}", text);
        }

        Ok(res.json()?)
    }

    pub fn delete_job(&self, id: &str) -> Result<bool> {
        let url = self.url(&format!("/jobs/{id}"), Some(&format!("/jobs/{id}")))?;
        let res = self.request_builder(reqwest::Method::DELETE, &url)?.send()?;

        if !res.status().is_success() {
            let text = res.text()?;
            anyhow::bail!("Failed to delete job: {}", text);
        }

        Ok(res.json()?)
    }

    // Schedules
    pub fn list_schedules(&self) -> Result<Vec<JobSchedule>> {
        let url = self.url("/schedules", None)?;
        let res = self.request_builder(reqwest::Method::GET, &url)?.send()?;

        if !res.status().is_success() {
            let text = res.text()?;
            anyhow::bail!("Failed to list schedules: {}", text);
        }

        Ok(res.json()?)
    }

    pub fn create_schedule(
        &self,
        name: String,
        cron: String,
        function_id: String,
        params: serde_json::Value,
    ) -> Result<String> {
        let url = self.url("/schedules", None)?;
        let payload = CreateScheduleRequest {
            name,
            cron_expression: cron,
            function_id,
            default_params: params,
        };

        let res = self.client.post(&url).json(&payload).send()?;

        if !res.status().is_success() {
            let text = res.text()?;
            anyhow::bail!("Failed to create schedule: {}", text);
        }

        // The API returns the ID as a string (Json<String>)
        Ok(res.json()?)
    }

    pub fn delete_schedule(&self, id: &str) -> Result<bool> {
        let url = self.url(&format!("/schedules/{id}"), None)?;
        let res = self.client.delete(&url).send()?;

        if !res.status().is_success() {
            let text = res.text()?;
            anyhow::bail!("Failed to delete schedule: {}", text);
        }

        Ok(res.json()?)
    }

    // OpenAPI Fragments
    #[allow(dead_code)]
    pub fn list_openapi_fragments(&self, domain: Option<&str>) -> Result<Vec<OpenApiFragment>> {
        let mut url = self.url("/openapi/fragments", None)?;
        if let Some(d) = domain {
            url = format!("{}?domain={}", url, d);
        }
        let res = self.request_builder(reqwest::Method::GET, &url)?.send()?;

        if !res.status().is_success() {
            let text = res.text()?;
            anyhow::bail!("Failed to list OpenAPI fragments: {}", text);
        }

        Ok(res.json()?)
    }

    pub fn upsert_openapi_fragment(
        &self,
        function_id: String,
        domain: String,
        fragment: serde_json::Value,
    ) -> Result<OpenApiFragment> {
        let url = self.url("/openapi/fragments", Some("/openapi/fragments"))?;
        let payload = CreateOpenApiFragmentRequest {
            function_id,
            domain,
            fragment,
        };
        let res = self
            .request_builder(reqwest::Method::POST, &url)?
            .json(&payload)
            .send()?;

        if !res.status().is_success() {
            let text = res.text()?;
            anyhow::bail!("Failed to upsert OpenAPI fragment: {}", text);
        }

        Ok(res.json()?)
    }

    #[allow(dead_code)]
    pub fn delete_openapi_fragment(&self, id: &str) -> Result<bool> {
        let url = self.url(&format!("/openapi/fragments/{id}"), None)?;
        let res = self.request_builder(reqwest::Method::DELETE, &url)?.send()?;

        if !res.status().is_success() {
            let text = res.text()?;
            anyhow::bail!("Failed to delete OpenAPI fragment: {}", text);
        }

        Ok(res.json()?)
    }

    #[allow(dead_code)]
    pub fn get_aggregated_spec(&self, domain: &str) -> Result<AggregatedSpec> {
        let url = self.url(&format!("/openapi/{domain}.json"), None)?;
        let res = self.request_builder(reqwest::Method::GET, &url)?.send()?;

        if !res.status().is_success() {
            let text = res.text()?;
            anyhow::bail!(
                "Failed to get aggregated spec for domain '{}': {}",
                domain,
                text
            );
        }

        Ok(res.json()?)
    }

    // Control Plane: list functions (returns UUID + function_key)
    pub fn list_compute_functions(&self) -> Result<Vec<ComputeFunction>> {
        let url = self.url("/registry/functions", Some("/functions"))?;
        let res = self.request_builder(reqwest::Method::GET, &url)?.send()?;

        if !res.status().is_success() {
            let text = res.text()?;
            anyhow::bail!("Failed to list compute functions: {}", text);
        }

        Ok(res.json()?)
    }

    // Route Bindings
    pub fn list_route_bindings(&self) -> Result<Vec<RouteBinding>> {
        let url = self.url("/bindings/routes", Some("/route-bindings"))?;
        let res = self.request_builder(reqwest::Method::GET, &url)?.send()?;

        if !res.status().is_success() {
            let text = res.text()?;
            anyhow::bail!("Failed to list route bindings: {}", text);
        }

        Ok(res.json()?)
    }

    pub fn create_route_binding(
        &self,
        path_pattern: String,
        http_method: String,
        function_id: String,
        auth_mode: Option<String>,
        required_permissions: Option<Vec<String>>,
        source: Option<String>,
    ) -> Result<RouteBinding> {
        let url = self.url("/bindings/routes", Some("/route-bindings"))?;
        let payload = CreateRouteBindingRequest {
            path_pattern,
            http_method: Some(http_method),
            function_id,
            version_id: None,
            policy_id: None,
            auth_mode,
            required_permissions,
            source,
        };
        let res = self
            .request_builder(reqwest::Method::POST, &url)?
            .json(&payload)
            .send()?;

        if !res.status().is_success() {
            let text = res.text()?;
            anyhow::bail!("Failed to create route binding: {}", text);
        }

        Ok(res.json()?)
    }

    pub fn delete_route_binding(&self, id: &str) -> Result<bool> {
        let url = self.url(&format!("/bindings/routes/{id}"), Some(&format!("/route-bindings/{id}")))?;
        let res = self.request_builder(reqwest::Method::DELETE, &url)?.send()?;

        if !res.status().is_success() {
            let text = res.text()?;
            anyhow::bail!("Failed to delete route binding: {}", text);
        }

        Ok(res.json()?)
    }
}
