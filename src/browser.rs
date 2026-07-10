//! Optional browser surface abstraction.
//!
//! This module deliberately has no GPUI, terminal, or native WebView imports.
//! A host can embed a `BrowserSurface` in a workspace split, while this layer
//! owns only stable IDs, routing, policy, and bounded automation. The `browser`
//! Cargo feature enables the abstraction; it does **not** claim that a native
//! WKWebView, WebView2, or WebKitGTK adapter is linked.

use std::{collections::BTreeMap, time::Duration};

use crate::{
    control::{
        BrowserAccessibilityNode, BrowserAccessibilitySnapshot, BrowserAutomationError,
        BrowserAutomationErrorCode, BrowserBackendCapability, BrowserBackendKind,
        BrowserBackendPreference, BrowserBackendStatus, BrowserCapabilities, BrowserConsoleEntry,
        BrowserConsoleResult, BrowserCookie, BrowserCookiesResult, BrowserDomAction,
        BrowserDownloadPolicy, BrowserDownloadResult, BrowserDownloadState,
        BrowserInteractionResult, BrowserJavaScriptResult, BrowserNavigationResult, BrowserNodeId,
        BrowserOriginStorage, BrowserScreenshot, BrowserStorageEntry, BrowserStorageState,
        BrowserSurfaceInfo, BrowserSurfaceOptions, BrowserTarget, Capabilities, ControlCommand,
        ControlError, ControlResult, MAX_BROWSER_CONSOLE_ENTRIES, MAX_BROWSER_COOKIES,
        MAX_BROWSER_ORIGINS, MAX_BROWSER_RESULT_BYTES, MAX_BROWSER_SCREENSHOT_BYTES,
        MAX_BROWSER_SCRIPT_BYTES, MAX_BROWSER_SNAPSHOT_NODES, MAX_BROWSER_URL_BYTES,
        MAX_REQUEST_TIMEOUT, SurfaceId, SurfaceKind, SurfaceSummary,
    },
    notifications::WorkspaceId,
};

/// A host-specific browser implementation. Native adapters own framework
/// lifecycle and rendering; this trait keeps automation/data operations away
/// from the terminal core and makes them testable with [`MockBrowserBackend`].
pub trait BrowserBackend: Send {
    fn kind(&self) -> BrowserBackendKind;
    fn title(&self) -> &str;
    fn current_url(&self) -> &str;

    fn navigate(
        &mut self,
        url: String,
        timeout: Duration,
    ) -> Result<BrowserNavigationResult, BrowserAutomationError>;
    fn accessibility_snapshot(
        &mut self,
        max_nodes: usize,
        timeout: Duration,
    ) -> Result<BrowserAccessibilitySnapshot, BrowserAutomationError>;
    fn interact(
        &mut self,
        target: BrowserTarget,
        action: BrowserDomAction,
        timeout: Duration,
    ) -> Result<BrowserInteractionResult, BrowserAutomationError>;
    fn evaluate_javascript(
        &mut self,
        script: String,
        timeout: Duration,
    ) -> Result<BrowserJavaScriptResult, BrowserAutomationError>;
    fn screenshot(
        &mut self,
        timeout: Duration,
    ) -> Result<BrowserScreenshot, BrowserAutomationError>;
    fn console(
        &mut self,
        limit: usize,
        timeout: Duration,
    ) -> Result<BrowserConsoleResult, BrowserAutomationError>;
    fn cookies(
        &mut self,
        limit: usize,
        timeout: Duration,
    ) -> Result<BrowserCookiesResult, BrowserAutomationError>;
    fn storage_state(
        &mut self,
        max_origins: usize,
        timeout: Duration,
    ) -> Result<BrowserStorageState, BrowserAutomationError>;
    fn request_download(
        &mut self,
        url: String,
        suggested_filename: Option<String>,
        timeout: Duration,
    ) -> Result<BrowserDownloadResult, BrowserAutomationError>;
}

/// Factory seam for an optional native adapter. A factory can advertise its
/// absence without linking any WebView libraries; `create` must return a typed
/// error rather than attempting a best-effort fallback.
pub trait BrowserBackendFactory: Send + Sync {
    fn kind(&self) -> BrowserBackendKind;
    fn availability(&self) -> BrowserBackendCapability;
    fn automation_capabilities(&self) -> BrowserCapabilities {
        BrowserCapabilities::default()
    }
    fn create(
        &self,
        options: &BrowserSurfaceOptions,
    ) -> Result<Box<dyn BrowserBackend>, BrowserAutomationError>;
}

/// The stable identity a workspace/split host needs to associate a native view
/// with an optional browser surface. It intentionally contains no UI entity
/// ID, so it remains valid across a split detach/reattach.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BrowserSurfaceRoute {
    pub workspace_id: WorkspaceId,
    pub surface_id: SurfaceId,
}

/// A browser surface is independent of terminal state. Workspace code may use
/// its route to put a native view in a split, and control code uses the same ID
/// to route automation commands.
pub struct BrowserSurface {
    id: SurfaceId,
    workspace_id: WorkspaceId,
    policy: BrowserSurfaceOptions,
    backend: Box<dyn BrowserBackend>,
}

impl BrowserSurface {
    fn new(
        id: SurfaceId,
        workspace_id: WorkspaceId,
        policy: BrowserSurfaceOptions,
        backend: Box<dyn BrowserBackend>,
    ) -> Self {
        Self {
            id,
            workspace_id,
            policy,
            backend,
        }
    }

    pub fn route(&self) -> BrowserSurfaceRoute {
        BrowserSurfaceRoute {
            workspace_id: self.workspace_id,
            surface_id: self.id,
        }
    }

    pub fn id(&self) -> SurfaceId {
        self.id
    }

    pub fn workspace_id(&self) -> WorkspaceId {
        self.workspace_id
    }

    pub fn policy(&self) -> &BrowserSurfaceOptions {
        &self.policy
    }

    pub fn backend_kind(&self) -> BrowserBackendKind {
        self.backend.kind()
    }

    fn info(&self, active: bool) -> BrowserSurfaceInfo {
        let (title, truncated) = truncate_utf8(self.backend.title(), MAX_BROWSER_RESULT_BYTES);
        BrowserSurfaceInfo {
            surface: SurfaceSummary {
                id: self.id,
                workspace_id: self.workspace_id,
                kind: SurfaceKind::Browser,
                active,
                title,
            },
            backend: self.backend.kind(),
            url: self.backend.current_url().to_string(),
            policy: self.policy.clone(),
            truncated,
        }
    }
}

/// Browser-side surface registry. A UI host owns visual attachment and split
/// layout; the registry deliberately only owns identity/routing and backend
/// lifecycle. This lets terminal-only applications omit the entire module.
pub struct BrowserSurfaceRegistry {
    next_surface_id: SurfaceId,
    factories: BTreeMap<BrowserBackendKind, Box<dyn BrowserBackendFactory>>,
    surfaces: BTreeMap<SurfaceId, BrowserSurface>,
    active_by_workspace: BTreeMap<WorkspaceId, SurfaceId>,
}

impl Default for BrowserSurfaceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl BrowserSurfaceRegistry {
    /// Create a registry with named native seams. None is reported operational
    /// until an embedding host registers a real backend factory.
    pub fn new() -> Self {
        let mut registry = Self {
            next_surface_id: 1,
            factories: BTreeMap::new(),
            surfaces: BTreeMap::new(),
            active_by_workspace: BTreeMap::new(),
        };
        registry.register_backend(WkWebViewBackendFactory);
        registry.register_backend(WebView2BackendFactory);
        registry.register_backend(WebKitGtkBackendFactory);
        registry
    }

    /// Convenience constructor for deterministic integration tests. Production
    /// callers must opt into this explicitly; a mock is never silently used as
    /// a real browser backend.
    pub fn with_mock_backend(fixture: MockBrowserFixture) -> Self {
        let mut registry = Self::new();
        registry.register_backend(MockBrowserBackendFactory::new(fixture));
        registry
    }

    pub fn register_backend(&mut self, factory: impl BrowserBackendFactory + 'static) {
        self.factories.insert(factory.kind(), Box::new(factory));
    }

    /// Merge browser capability discovery into the common control API without
    /// making the terminal control handler depend on any browser backend.
    pub fn augment_capabilities(&self, capabilities: &mut Capabilities) {
        let browser = self.capabilities();
        capabilities.browser_surfaces = browser.available;
        capabilities.browser = browser;
    }

    pub fn capabilities(&self) -> BrowserCapabilities {
        let mut capabilities = BrowserCapabilities::default();

        for factory in self.factories.values() {
            let backend = factory.availability();
            if backend.status == BrowserBackendStatus::Available {
                let advertised = factory.automation_capabilities();
                capabilities.available = true;
                capabilities.navigation |= advertised.navigation;
                capabilities.accessibility_snapshot |= advertised.accessibility_snapshot;
                capabilities.dom_interaction |= advertised.dom_interaction;
                capabilities.javascript |= advertised.javascript;
                capabilities.screenshots |= advertised.screenshots;
                capabilities.console |= advertised.console;
                capabilities.cookies |= advertised.cookies;
                capabilities.storage |= advertised.storage;
                capabilities.downloads |= advertised.downloads;
            }
            capabilities.backends.push(backend);
        }

        capabilities
    }

    pub fn create_surface(
        &mut self,
        workspace_id: WorkspaceId,
        options: BrowserSurfaceOptions,
    ) -> Result<BrowserSurfaceInfo, BrowserAutomationError> {
        options.validate()?;
        let factory = self.select_factory(options.backend)?;
        let backend = factory.create(&options)?;
        let id = self.next_surface_id;
        self.next_surface_id = self.next_surface_id.saturating_add(1);

        let surface = BrowserSurface::new(id, workspace_id, options, backend);
        let info = surface.info(true);
        self.surfaces.insert(id, surface);
        self.active_by_workspace.insert(workspace_id, id);
        Ok(info)
    }

    pub fn surface(
        &self,
        workspace_id: WorkspaceId,
        surface_id: SurfaceId,
    ) -> Option<&BrowserSurface> {
        self.surfaces
            .get(&surface_id)
            .filter(|surface| surface.workspace_id == workspace_id)
    }

    pub fn route(
        &self,
        workspace_id: WorkspaceId,
        surface_id: SurfaceId,
    ) -> Option<BrowserSurfaceRoute> {
        self.surface(workspace_id, surface_id)
            .map(BrowserSurface::route)
    }

    pub fn surface_summaries(&self, workspace_id: WorkspaceId) -> Vec<SurfaceSummary> {
        self.surfaces
            .values()
            .filter(|surface| surface.workspace_id == workspace_id)
            .map(|surface| {
                surface
                    .info(self.active_by_workspace.get(&workspace_id) == Some(&surface.id))
                    .surface
            })
            .collect()
    }

    /// Hosts call this after routing a split/focus change. It does not claim to
    /// focus a native view itself; that remains the platform host's job.
    pub fn set_active(
        &mut self,
        workspace_id: WorkspaceId,
        surface_id: SurfaceId,
    ) -> Result<(), BrowserAutomationError> {
        self.ensure_surface(workspace_id, surface_id)?;
        self.active_by_workspace.insert(workspace_id, surface_id);
        Ok(())
    }

    pub fn remove_surface(
        &mut self,
        workspace_id: WorkspaceId,
        surface_id: SurfaceId,
    ) -> Result<BrowserSurface, BrowserAutomationError> {
        self.ensure_surface(workspace_id, surface_id)?;
        let surface = self
            .surfaces
            .remove(&surface_id)
            .expect("surface existence checked immediately before removal");
        if self.active_by_workspace.get(&workspace_id) == Some(&surface_id) {
            self.active_by_workspace.remove(&workspace_id);
        }
        Ok(surface)
    }

    /// Delegate a browser-specific control command. `None` means the command
    /// belongs to the terminal/workspace side of a composite control handler.
    pub fn handle_control_command(
        &mut self,
        command: ControlCommand,
        timeout: Duration,
    ) -> Option<Result<ControlResult, ControlError>> {
        if let ControlCommand::SurfaceScreenshot {
            workspace_id,
            surface_id,
        } = &command
            && self.surface(*workspace_id, *surface_id).is_none()
        {
            return None;
        }

        if !command.is_browser_command()
            && !matches!(command, ControlCommand::SurfaceScreenshot { .. })
        {
            return None;
        }

        if let Err(error) = command.validate() {
            return Some(Err(error));
        }

        let result = (|| -> Result<ControlResult, BrowserAutomationError> {
            match command {
                ControlCommand::SurfaceCreateBrowser {
                    workspace_id,
                    options,
                } => Ok(ControlResult::BrowserSurface(
                    self.create_surface(workspace_id, options)?,
                )),
                ControlCommand::BrowserGetInfo {
                    workspace_id,
                    surface_id,
                } => Ok(ControlResult::BrowserSurface(
                    self.surface_info(workspace_id, surface_id)?,
                )),
                ControlCommand::BrowserNavigate {
                    workspace_id,
                    surface_id,
                    url,
                } => {
                    let surface = self.surface_mut(workspace_id, surface_id)?;
                    Ok(ControlResult::BrowserNavigation(
                        surface.backend.navigate(url, timeout)?,
                    ))
                }
                ControlCommand::BrowserAccessibilitySnapshot {
                    workspace_id,
                    surface_id,
                    max_nodes,
                } => {
                    let surface = self.surface_mut(workspace_id, surface_id)?;
                    Ok(ControlResult::BrowserAccessibilitySnapshot(
                        surface.backend.accessibility_snapshot(max_nodes, timeout)?,
                    ))
                }
                ControlCommand::BrowserInteract {
                    workspace_id,
                    surface_id,
                    target,
                    action,
                } => {
                    let surface = self.surface_mut(workspace_id, surface_id)?;
                    Ok(ControlResult::BrowserInteraction(
                        surface.backend.interact(target, action, timeout)?,
                    ))
                }
                ControlCommand::BrowserEvaluateJavaScript {
                    workspace_id,
                    surface_id,
                    script,
                } => {
                    let surface = self.surface_mut(workspace_id, surface_id)?;
                    Ok(ControlResult::BrowserJavaScript(
                        surface.backend.evaluate_javascript(script, timeout)?,
                    ))
                }
                ControlCommand::SurfaceScreenshot {
                    workspace_id,
                    surface_id,
                } => {
                    let surface = self.surface_mut(workspace_id, surface_id)?;
                    let screenshot = surface.backend.screenshot(timeout)?;
                    Ok(ControlResult::Screenshot {
                        mime_type: screenshot.mime_type,
                        data_base64: screenshot.data_base64,
                        truncated: screenshot.truncated,
                    })
                }
                ControlCommand::BrowserConsoleList {
                    workspace_id,
                    surface_id,
                    limit,
                } => {
                    let surface = self.surface_mut(workspace_id, surface_id)?;
                    Ok(ControlResult::BrowserConsole(
                        surface.backend.console(limit, timeout)?,
                    ))
                }
                ControlCommand::BrowserCookieList {
                    workspace_id,
                    surface_id,
                    limit,
                } => {
                    let surface = self.surface_mut(workspace_id, surface_id)?;
                    Ok(ControlResult::BrowserCookies(
                        surface.backend.cookies(limit, timeout)?,
                    ))
                }
                ControlCommand::BrowserStorageState {
                    workspace_id,
                    surface_id,
                    max_origins,
                } => {
                    let surface = self.surface_mut(workspace_id, surface_id)?;
                    Ok(ControlResult::BrowserStorage(
                        surface.backend.storage_state(max_origins, timeout)?,
                    ))
                }
                ControlCommand::BrowserDownload {
                    workspace_id,
                    surface_id,
                    url,
                    suggested_filename,
                } => {
                    let surface = self.surface_mut(workspace_id, surface_id)?;
                    Ok(ControlResult::BrowserDownload(
                        surface
                            .backend
                            .request_download(url, suggested_filename, timeout)?,
                    ))
                }
                _ => unreachable!("browser command filter above is exhaustive"),
            }
        })();

        Some(result.map_err(Into::into))
    }

    fn select_factory(
        &self,
        preference: BrowserBackendPreference,
    ) -> Result<&dyn BrowserBackendFactory, BrowserAutomationError> {
        let requested = match preference {
            BrowserBackendPreference::Auto => self
                .factories
                .values()
                .find(|factory| factory.availability().status == BrowserBackendStatus::Available)
                .map(|factory| factory.as_ref()),
            BrowserBackendPreference::Mock => self
                .factories
                .get(&BrowserBackendKind::Mock)
                .map(|factory| factory.as_ref()),
            BrowserBackendPreference::WkWebView => self
                .factories
                .get(&BrowserBackendKind::WkWebView)
                .map(|factory| factory.as_ref()),
            BrowserBackendPreference::WebView2 => self
                .factories
                .get(&BrowserBackendKind::WebView2)
                .map(|factory| factory.as_ref()),
            BrowserBackendPreference::WebKitGtk => self
                .factories
                .get(&BrowserBackendKind::WebKitGtk)
                .map(|factory| factory.as_ref()),
        };

        let Some(factory) = requested else {
            return Err(BrowserAutomationError::new(
                BrowserAutomationErrorCode::BackendUnavailable,
                "no requested browser backend is registered in this host",
                false,
            ));
        };

        let availability = factory.availability();
        if availability.status != BrowserBackendStatus::Available {
            return Err(BrowserAutomationError::new(
                BrowserAutomationErrorCode::BackendUnavailable,
                availability.detail.unwrap_or_else(|| {
                    format!("{:?} is not available in this build", availability.backend)
                }),
                false,
            ));
        }

        Ok(factory)
    }

    fn ensure_surface(
        &self,
        workspace_id: WorkspaceId,
        surface_id: SurfaceId,
    ) -> Result<(), BrowserAutomationError> {
        if self.surface(workspace_id, surface_id).is_none() {
            return Err(BrowserAutomationError::new(
                BrowserAutomationErrorCode::TargetNotFound,
                format!("browser surface {surface_id} was not found in workspace {workspace_id}"),
                false,
            ));
        }
        Ok(())
    }

    fn surface_mut(
        &mut self,
        workspace_id: WorkspaceId,
        surface_id: SurfaceId,
    ) -> Result<&mut BrowserSurface, BrowserAutomationError> {
        self.ensure_surface(workspace_id, surface_id)?;
        Ok(self
            .surfaces
            .get_mut(&surface_id)
            .expect("surface existence checked immediately before mutable lookup"))
    }

    fn surface_info(
        &self,
        workspace_id: WorkspaceId,
        surface_id: SurfaceId,
    ) -> Result<BrowserSurfaceInfo, BrowserAutomationError> {
        let surface = self.surface(workspace_id, surface_id).ok_or_else(|| {
            BrowserAutomationError::new(
                BrowserAutomationErrorCode::TargetNotFound,
                format!("browser surface {surface_id} was not found in workspace {workspace_id}"),
                false,
            )
        })?;
        Ok(surface.info(self.active_by_workspace.get(&workspace_id) == Some(&surface_id)))
    }
}

/// Exact discovery information for native seam names. The structs below are
/// intentionally unavailable: they reserve integration points without making
/// a false claim that the required host framework or bindings are present.
pub fn platform_backend_capability(kind: BrowserBackendKind) -> BrowserBackendCapability {
    let (status, detail) = match kind {
        BrowserBackendKind::Mock => (
            BrowserBackendStatus::NotCompiled,
            "the mock backend must be registered explicitly".to_string(),
        ),
        BrowserBackendKind::WkWebView if cfg!(target_os = "macos") => (
            BrowserBackendStatus::NotCompiled,
            "WKWebView adapter seam is present, but no macOS WebKit binding is linked".to_string(),
        ),
        BrowserBackendKind::WkWebView => (
            BrowserBackendStatus::UnsupportedHost,
            "WKWebView is only a macOS host adapter".to_string(),
        ),
        BrowserBackendKind::WebView2 if cfg!(target_os = "windows") => (
            BrowserBackendStatus::NotCompiled,
            "WebView2 adapter seam is present, but no Windows WebView2 binding is linked"
                .to_string(),
        ),
        BrowserBackendKind::WebView2 => (
            BrowserBackendStatus::UnsupportedHost,
            "WebView2 is only a Windows host adapter".to_string(),
        ),
        BrowserBackendKind::WebKitGtk if cfg!(any(target_os = "linux", target_os = "freebsd")) => (
            BrowserBackendStatus::NotCompiled,
            "WebKitGTK adapter seam is present, but no WebKitGTK binding is linked".to_string(),
        ),
        BrowserBackendKind::WebKitGtk => (
            BrowserBackendStatus::UnsupportedHost,
            "WebKitGTK is only a Linux or FreeBSD host adapter".to_string(),
        ),
    };

    BrowserBackendCapability {
        backend: kind,
        status,
        detail: Some(detail),
    }
}

macro_rules! unavailable_native_factory {
    ($name:ident, $kind:expr) => {
        #[derive(Default)]
        pub struct $name;

        impl BrowserBackendFactory for $name {
            fn kind(&self) -> BrowserBackendKind {
                $kind
            }

            fn availability(&self) -> BrowserBackendCapability {
                platform_backend_capability($kind)
            }

            fn create(
                &self,
                _options: &BrowserSurfaceOptions,
            ) -> Result<Box<dyn BrowserBackend>, BrowserAutomationError> {
                let capability = self.availability();
                Err(BrowserAutomationError::new(
                    BrowserAutomationErrorCode::BackendUnavailable,
                    capability
                        .detail
                        .unwrap_or_else(|| "browser backend unavailable".to_string()),
                    false,
                ))
            }
        }
    };
}

unavailable_native_factory!(WkWebViewBackendFactory, BrowserBackendKind::WkWebView);
unavailable_native_factory!(WebView2BackendFactory, BrowserBackendKind::WebView2);
unavailable_native_factory!(WebKitGtkBackendFactory, BrowserBackendKind::WebKitGtk);

/// Data supplied to the deterministic mock backend. This is intentionally
/// public so callers can write fixture-backed integration tests without a
/// native WebView or network access.
#[derive(Clone, Debug)]
pub struct MockBrowserFixture {
    pub title: String,
    pub nodes: Vec<MockAccessibilityNode>,
    pub css_targets: BTreeMap<String, String>,
    pub javascript_results: BTreeMap<String, String>,
    pub screenshot: BrowserScreenshot,
    pub console: Vec<BrowserConsoleEntry>,
    pub storage: BrowserStorageState,
    /// Simulated completion latency. The mock never sleeps: it returns a typed
    /// timeout whenever this exceeds the request's bounded deadline.
    pub operation_delay: Duration,
}

impl Default for MockBrowserFixture {
    fn default() -> Self {
        let mut css_targets = BTreeMap::new();
        css_targets.insert("#submit".to_string(), "submit".to_string());
        let mut javascript_results = BTreeMap::new();
        javascript_results.insert("document.title".to_string(), "\"Mock page\"".to_string());
        javascript_results.insert("location.href".to_string(), "\"about:blank\"".to_string());

        Self {
            title: "Mock page".to_string(),
            nodes: vec![
                MockAccessibilityNode {
                    node_id: "root".to_string(),
                    parent_id: None,
                    role: "document".to_string(),
                    name: "Mock page".to_string(),
                    value: None,
                    description: None,
                },
                MockAccessibilityNode {
                    node_id: "submit".to_string(),
                    parent_id: Some("root".to_string()),
                    role: "button".to_string(),
                    name: "Submit".to_string(),
                    value: None,
                    description: Some("A deterministic mock button".to_string()),
                },
            ],
            css_targets,
            javascript_results,
            screenshot: BrowserScreenshot {
                mime_type: "image/png".to_string(),
                data_base64: "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADElEQVR42mNk+M/wHwAF/gL+V6Tp2QAAAABJRU5ErkJggg==".to_string(),
                truncated: false,
            },
            console: Vec::new(),
            storage: BrowserStorageState::default(),
            operation_delay: Duration::ZERO,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MockAccessibilityNode {
    pub node_id: String,
    pub parent_id: Option<String>,
    pub role: String,
    pub name: String,
    pub value: Option<String>,
    pub description: Option<String>,
}

pub struct MockBrowserBackendFactory {
    fixture: MockBrowserFixture,
}

impl MockBrowserBackendFactory {
    pub fn new(fixture: MockBrowserFixture) -> Self {
        Self { fixture }
    }
}

impl BrowserBackendFactory for MockBrowserBackendFactory {
    fn kind(&self) -> BrowserBackendKind {
        BrowserBackendKind::Mock
    }

    fn availability(&self) -> BrowserBackendCapability {
        BrowserBackendCapability {
            backend: BrowserBackendKind::Mock,
            status: BrowserBackendStatus::Available,
            detail: Some("deterministic test fixture backend".to_string()),
        }
    }

    fn automation_capabilities(&self) -> BrowserCapabilities {
        BrowserCapabilities {
            available: true,
            backends: Vec::new(),
            navigation: true,
            accessibility_snapshot: true,
            dom_interaction: true,
            javascript: true,
            screenshots: true,
            console: true,
            cookies: true,
            storage: true,
            downloads: true,
            max_timeout_ms: MAX_REQUEST_TIMEOUT.as_millis() as u32,
            max_snapshot_nodes: MAX_BROWSER_SNAPSHOT_NODES,
            max_result_bytes: MAX_BROWSER_RESULT_BYTES,
        }
    }

    fn create(
        &self,
        options: &BrowserSurfaceOptions,
    ) -> Result<Box<dyn BrowserBackend>, BrowserAutomationError> {
        options.validate()?;
        Ok(Box::new(MockBrowserBackend::new(
            self.fixture.clone(),
            options.clone(),
        )))
    }
}

/// Deterministic backend used only when explicitly registered. It models
/// document identity, stale targets, policies, and timeouts without network or
/// GUI dependencies, making automation behavior testable on every host.
pub struct MockBrowserBackend {
    fixture: MockBrowserFixture,
    policy: BrowserSurfaceOptions,
    current_url: String,
    document_generation: u64,
}

impl MockBrowserBackend {
    pub fn new(fixture: MockBrowserFixture, policy: BrowserSurfaceOptions) -> Self {
        Self {
            current_url: policy.initial_url.clone(),
            fixture,
            policy,
            document_generation: 1,
        }
    }

    fn document_id(&self) -> String {
        format!("mock-document-{}", self.document_generation)
    }

    fn before_operation(&self, timeout: Duration) -> Result<(), BrowserAutomationError> {
        if timeout.is_zero() || self.fixture.operation_delay > timeout {
            return Err(BrowserAutomationError::new(
                BrowserAutomationErrorCode::Timeout,
                format!(
                    "mock browser operation exceeded the {} ms request deadline",
                    timeout.as_millis()
                ),
                true,
            ));
        }
        Ok(())
    }

    fn resolve_target(
        &self,
        target: &BrowserTarget,
    ) -> Result<MockAccessibilityNode, BrowserAutomationError> {
        target.validate()?;
        let expected_document = match target {
            BrowserTarget::SnapshotNode { node } => &node.document_id,
            BrowserTarget::Accessibility { document_id, .. }
            | BrowserTarget::Css { document_id, .. } => document_id,
        };
        if expected_document != &self.document_id() {
            return Err(BrowserAutomationError::new(
                BrowserAutomationErrorCode::StaleTarget,
                "browser target belongs to a document that has already navigated",
                false,
            ));
        }

        let node_id = match target {
            BrowserTarget::SnapshotNode { node } => Some(node.node_id.as_str()),
            BrowserTarget::Accessibility {
                role, name, index, ..
            } => self
                .fixture
                .nodes
                .iter()
                .filter(|node| node.role == *role && node.name == *name)
                .nth(*index)
                .map(|node| node.node_id.as_str()),
            BrowserTarget::Css { selector, .. } => self.css_target(selector),
        };

        let Some(node_id) = node_id else {
            return Err(BrowserAutomationError::new(
                BrowserAutomationErrorCode::TargetNotFound,
                "browser target was not found in the current document",
                false,
            ));
        };
        self.fixture
            .nodes
            .iter()
            .find(|node| node.node_id == node_id)
            .cloned()
            .ok_or_else(|| {
                BrowserAutomationError::new(
                    BrowserAutomationErrorCode::TargetNotFound,
                    "browser target resolved to a missing fixture node",
                    false,
                )
            })
    }

    fn css_target(&self, selector: &str) -> Option<&str> {
        self.fixture.css_targets.get(selector).map(String::as_str)
    }

    fn browser_node_id(&self, node_id: &str) -> BrowserNodeId {
        BrowserNodeId {
            document_id: self.document_id(),
            node_id: node_id.to_string(),
        }
    }
}

impl BrowserBackend for MockBrowserBackend {
    fn kind(&self) -> BrowserBackendKind {
        BrowserBackendKind::Mock
    }

    fn title(&self) -> &str {
        &self.fixture.title
    }

    fn current_url(&self) -> &str {
        &self.current_url
    }

    fn navigate(
        &mut self,
        url: String,
        timeout: Duration,
    ) -> Result<BrowserNavigationResult, BrowserAutomationError> {
        validate_url(&url)?;
        self.before_operation(timeout)?;
        self.current_url = url;
        self.document_generation = self.document_generation.saturating_add(1);
        let (title, truncated) = truncate_utf8(&self.fixture.title, MAX_BROWSER_RESULT_BYTES);
        Ok(BrowserNavigationResult {
            url: self.current_url.clone(),
            document_id: self.document_id(),
            title,
            truncated,
        })
    }

    fn accessibility_snapshot(
        &mut self,
        max_nodes: usize,
        timeout: Duration,
    ) -> Result<BrowserAccessibilitySnapshot, BrowserAutomationError> {
        if max_nodes > MAX_BROWSER_SNAPSHOT_NODES {
            return Err(limit_error(
                "browser snapshot node limit",
                MAX_BROWSER_SNAPSHOT_NODES,
            ));
        }
        self.before_operation(timeout)?;
        let document_id = self.document_id();
        let (title, mut truncated) = truncate_utf8(
            &self.fixture.title,
            MAX_BROWSER_RESULT_BYTES.saturating_sub(document_id.len() + self.current_url.len()),
        );
        let mut remaining = MAX_BROWSER_RESULT_BYTES
            .saturating_sub(document_id.len() + self.current_url.len() + title.len());
        let mut nodes = Vec::new();
        for node in self.fixture.nodes.iter().take(max_nodes) {
            let candidate = BrowserAccessibilityNode {
                id: BrowserNodeId {
                    document_id: document_id.clone(),
                    node_id: node.node_id.clone(),
                },
                parent: node.parent_id.as_ref().map(|parent_id| BrowserNodeId {
                    document_id: document_id.clone(),
                    node_id: parent_id.clone(),
                }),
                role: node.role.clone(),
                name: node.name.clone(),
                value: node.value.clone(),
                description: node.description.clone(),
            };
            let size = accessibility_node_bytes(&candidate);
            if size > remaining {
                truncated = true;
                break;
            }
            remaining -= size;
            nodes.push(candidate);
        }
        truncated |= self.fixture.nodes.len() > max_nodes;
        Ok(BrowserAccessibilitySnapshot {
            document_id,
            url: self.current_url.clone(),
            title,
            nodes,
            truncated,
        })
    }

    fn interact(
        &mut self,
        target: BrowserTarget,
        action: BrowserDomAction,
        timeout: Duration,
    ) -> Result<BrowserInteractionResult, BrowserAutomationError> {
        action.validate()?;
        self.before_operation(timeout)?;
        let node = self.resolve_target(&target)?;
        Ok(BrowserInteractionResult {
            target: self.browser_node_id(&node.node_id),
            action,
            url: self.current_url.clone(),
        })
    }

    fn evaluate_javascript(
        &mut self,
        script: String,
        timeout: Duration,
    ) -> Result<BrowserJavaScriptResult, BrowserAutomationError> {
        if script.len() > MAX_BROWSER_SCRIPT_BYTES {
            return Err(limit_error("JavaScript source", MAX_BROWSER_SCRIPT_BYTES));
        }
        self.before_operation(timeout)?;
        let Some(value_json) = self.fixture.javascript_results.get(&script) else {
            return Err(BrowserAutomationError::new(
                BrowserAutomationErrorCode::JavaScriptFailed,
                "mock fixture has no result for the requested JavaScript source",
                false,
            ));
        };
        let (value_json, truncated) = truncate_utf8(value_json, MAX_BROWSER_RESULT_BYTES);
        Ok(BrowserJavaScriptResult {
            value_json,
            truncated,
        })
    }

    fn screenshot(
        &mut self,
        timeout: Duration,
    ) -> Result<BrowserScreenshot, BrowserAutomationError> {
        self.before_operation(timeout)?;
        if self.fixture.screenshot.data_base64.len() > MAX_BROWSER_SCREENSHOT_BYTES {
            return Err(limit_error(
                "browser screenshot payload",
                MAX_BROWSER_SCREENSHOT_BYTES,
            ));
        }
        Ok(self.fixture.screenshot.clone())
    }

    fn console(
        &mut self,
        limit: usize,
        timeout: Duration,
    ) -> Result<BrowserConsoleResult, BrowserAutomationError> {
        if limit > MAX_BROWSER_CONSOLE_ENTRIES {
            return Err(limit_error(
                "browser console limit",
                MAX_BROWSER_CONSOLE_ENTRIES,
            ));
        }
        self.before_operation(timeout)?;
        let truncated = self.fixture.console.len() > limit;
        let mut consumed = 0usize;
        let mut entries = Vec::new();
        let mut output_truncated = truncated;
        for entry in self.fixture.console.iter().take(limit) {
            let remaining = MAX_BROWSER_RESULT_BYTES.saturating_sub(consumed);
            if remaining == 0 {
                output_truncated = true;
                break;
            }
            let mut entry = entry.clone();
            let (source, source_truncated) = match entry.source.take() {
                Some(source) => {
                    let (source, truncated) = truncate_utf8(&source, remaining);
                    (Some(source), truncated)
                }
                None => (None, false),
            };
            consumed = consumed.saturating_add(source.as_deref().map_or(0, str::len));
            let remaining = MAX_BROWSER_RESULT_BYTES.saturating_sub(consumed);
            let (message, clipped) = truncate_utf8(&entry.message, remaining);
            consumed = consumed.saturating_add(message.len());
            entry.source = source;
            entry.message = message;
            entries.push(entry);
            output_truncated |= source_truncated || clipped;
            if source_truncated || clipped {
                break;
            }
        }
        Ok(BrowserConsoleResult {
            entries,
            truncated: output_truncated,
        })
    }

    fn cookies(
        &mut self,
        limit: usize,
        timeout: Duration,
    ) -> Result<BrowserCookiesResult, BrowserAutomationError> {
        if limit > MAX_BROWSER_COOKIES {
            return Err(limit_error("browser cookie limit", MAX_BROWSER_COOKIES));
        }
        self.before_operation(timeout)?;
        let (cookies, truncated) = bounded_cookies(&self.fixture.storage.cookies, limit);
        Ok(BrowserCookiesResult { cookies, truncated })
    }

    fn storage_state(
        &mut self,
        max_origins: usize,
        timeout: Duration,
    ) -> Result<BrowserStorageState, BrowserAutomationError> {
        if max_origins > MAX_BROWSER_ORIGINS {
            return Err(limit_error("browser origin limit", MAX_BROWSER_ORIGINS));
        }
        self.before_operation(timeout)?;
        let (cookies, mut truncated) =
            bounded_cookies(&self.fixture.storage.cookies, MAX_BROWSER_COOKIES);
        let mut remaining = MAX_BROWSER_RESULT_BYTES
            .saturating_sub(cookies.iter().map(cookie_bytes).sum::<usize>());
        let mut origins = Vec::new();
        for origin in self.fixture.storage.origins.iter().take(max_origins) {
            let (bounded, used, origin_truncated) = bounded_origin(origin, remaining);
            remaining = remaining.saturating_sub(used);
            origins.push(bounded);
            truncated |= origin_truncated;
            if remaining == 0 {
                truncated = true;
                break;
            }
        }
        truncated |= self.fixture.storage.origins.len() > max_origins;
        truncated |= self.fixture.storage.truncated;
        Ok(BrowserStorageState {
            cookies,
            origins,
            truncated,
        })
    }

    fn request_download(
        &mut self,
        url: String,
        suggested_filename: Option<String>,
        timeout: Duration,
    ) -> Result<BrowserDownloadResult, BrowserAutomationError> {
        validate_url(&url)?;
        self.before_operation(timeout)?;
        let BrowserDownloadPolicy::AllowTo { directory } = &self.policy.downloads else {
            return Err(BrowserAutomationError::new(
                BrowserAutomationErrorCode::DownloadDenied,
                "browser download policy denies downloads for this surface",
                false,
            ));
        };
        let filename = suggested_filename
            .as_deref()
            .map(safe_filename)
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| filename_from_url(&url));
        Ok(BrowserDownloadResult {
            url,
            filename: filename.clone(),
            // The mock validates policy and routing only. It never creates a
            // file, so `accepted` cannot be misconstrued as a completed write.
            state: BrowserDownloadState::Accepted,
            destination: Some(format!("{}/{}", directory.trim_end_matches('/'), filename)),
        })
    }
}

fn validate_url(url: &str) -> Result<(), BrowserAutomationError> {
    if url.is_empty() {
        return Err(BrowserAutomationError::new(
            BrowserAutomationErrorCode::NavigationFailed,
            "browser URL must not be empty",
            false,
        ));
    }
    if url.len() > MAX_BROWSER_URL_BYTES {
        return Err(limit_error("browser URL", MAX_BROWSER_URL_BYTES));
    }
    Ok(())
}

fn limit_error(name: &str, maximum: usize) -> BrowserAutomationError {
    BrowserAutomationError::new(
        BrowserAutomationErrorCode::LimitExceeded,
        format!("{name} exceeds the {maximum}-item or byte limit"),
        false,
    )
}

fn truncate_utf8(value: &str, max_bytes: usize) -> (String, bool) {
    if value.len() <= max_bytes {
        return (value.to_string(), false);
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    (value[..end].to_string(), true)
}

fn bounded_cookies(cookies: &[BrowserCookie], limit: usize) -> (Vec<BrowserCookie>, bool) {
    let mut remaining = MAX_BROWSER_RESULT_BYTES;
    let mut result = Vec::new();
    let mut truncated = cookies.len() > limit;
    for cookie in cookies.iter().take(limit) {
        let size = cookie_bytes(cookie);
        if size > remaining {
            truncated = true;
            break;
        }
        remaining -= size;
        result.push(cookie.clone());
    }
    (result, truncated)
}

fn cookie_bytes(cookie: &BrowserCookie) -> usize {
    cookie.name.len()
        + cookie.value.len()
        + cookie.domain.len()
        + cookie.path.len()
        + cookie.same_site.as_deref().map_or(0, str::len)
}

fn accessibility_node_bytes(node: &BrowserAccessibilityNode) -> usize {
    node.id.document_id.len()
        + node.id.node_id.len()
        + node
            .parent
            .as_ref()
            .map_or(0, |parent| parent.document_id.len() + parent.node_id.len())
        + node.role.len()
        + node.name.len()
        + node.value.as_deref().map_or(0, str::len)
        + node.description.as_deref().map_or(0, str::len)
}

fn bounded_origin(
    origin: &BrowserOriginStorage,
    remaining: usize,
) -> (BrowserOriginStorage, usize, bool) {
    let (name, name_truncated) = truncate_utf8(&origin.origin, remaining);
    let mut used = name.len();
    let (local_storage, local_used, local_truncated) =
        bounded_entries(&origin.local_storage, remaining.saturating_sub(used));
    used += local_used;
    let (session_storage, session_used, session_truncated) =
        bounded_entries(&origin.session_storage, remaining.saturating_sub(used));
    used += session_used;
    (
        BrowserOriginStorage {
            origin: name,
            local_storage,
            session_storage,
        },
        used,
        name_truncated || local_truncated || session_truncated,
    )
}

fn bounded_entries(
    entries: &[BrowserStorageEntry],
    remaining: usize,
) -> (Vec<BrowserStorageEntry>, usize, bool) {
    let mut result = Vec::new();
    let mut used = 0usize;
    for entry in entries {
        let size = entry.key.len() + entry.value.len();
        if size > remaining.saturating_sub(used) {
            return (result, used, true);
        }
        used += size;
        result.push(entry.clone());
    }
    (result, used, false)
}

fn filename_from_url(url: &str) -> String {
    url.rsplit('/')
        .next()
        .map(safe_filename)
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "download".to_string())
}

fn safe_filename(value: &str) -> String {
    value
        .chars()
        .filter(|character| *character != '/' && *character != '\\' && *character != '\0')
        .take(255)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{
        CONTROL_PROTOCOL_VERSION, ControlErrorCode, ControlRequest, dispatch_frame,
    };

    fn browser_request(id: u64, command: ControlCommand) -> ControlRequest {
        ControlRequest {
            version: CONTROL_PROTOCOL_VERSION,
            id,
            timeout_ms: Some(100),
            command,
        }
    }

    struct RegistryHandler {
        registry: BrowserSurfaceRegistry,
    }

    impl crate::control::ControlHandler for RegistryHandler {
        fn handle(
            &mut self,
            command: ControlCommand,
            timeout: Duration,
        ) -> Result<ControlResult, ControlError> {
            self.registry
                .handle_control_command(command, timeout)
                .unwrap_or_else(|| {
                    Err(ControlError::new(
                        ControlErrorCode::NotSupported,
                        "not handled by the browser fixture",
                    ))
                })
        }
    }

    #[test]
    fn terminal_only_capability_is_honest_until_a_backend_is_registered() {
        let registry = BrowserSurfaceRegistry::new();
        let mut capabilities = Capabilities::default();
        registry.augment_capabilities(&mut capabilities);

        assert!(!capabilities.browser_surfaces);
        assert!(!capabilities.browser.available);
        assert!(
            capabilities
                .browser
                .backends
                .iter()
                .all(|backend| backend.status != BrowserBackendStatus::Available)
        );
    }

    #[test]
    fn mock_fixture_exposes_stable_targets_and_workspace_routing() {
        let mut registry = BrowserSurfaceRegistry::with_mock_backend(MockBrowserFixture::default());
        let info = registry
            .create_surface(
                7,
                BrowserSurfaceOptions {
                    backend: BrowserBackendPreference::Mock,
                    ..Default::default()
                },
            )
            .unwrap();

        assert_eq!(info.surface.kind, SurfaceKind::Browser);
        assert_eq!(registry.route(7, info.surface.id).unwrap().workspace_id, 7);
        assert_eq!(registry.surface_summaries(7), vec![info.surface.clone()]);

        let snapshot = match registry
            .handle_control_command(
                ControlCommand::BrowserAccessibilitySnapshot {
                    workspace_id: 7,
                    surface_id: info.surface.id,
                    max_nodes: 10,
                },
                Duration::from_millis(100),
            )
            .unwrap()
            .unwrap()
        {
            ControlResult::BrowserAccessibilitySnapshot(snapshot) => snapshot,
            result => panic!("expected snapshot, got {result:?}"),
        };
        let target = snapshot
            .nodes
            .iter()
            .find(|node| node.name == "Submit")
            .unwrap()
            .id
            .clone();

        let interaction = registry
            .handle_control_command(
                ControlCommand::BrowserInteract {
                    workspace_id: 7,
                    surface_id: info.surface.id,
                    target: BrowserTarget::SnapshotNode {
                        node: target.clone(),
                    },
                    action: BrowserDomAction::Click,
                },
                Duration::from_millis(100),
            )
            .unwrap()
            .unwrap();
        assert!(matches!(interaction, ControlResult::BrowserInteraction(_)));

        let navigation = registry
            .handle_control_command(
                ControlCommand::BrowserNavigate {
                    workspace_id: 7,
                    surface_id: info.surface.id,
                    url: "https://example.test/next".to_string(),
                },
                Duration::from_millis(100),
            )
            .unwrap()
            .unwrap();
        assert!(matches!(navigation, ControlResult::BrowserNavigation(_)));

        let stale = registry
            .handle_control_command(
                ControlCommand::BrowserInteract {
                    workspace_id: 7,
                    surface_id: info.surface.id,
                    target: BrowserTarget::SnapshotNode { node: target },
                    action: BrowserDomAction::Click,
                },
                Duration::from_millis(100),
            )
            .unwrap()
            .unwrap_err();
        assert_eq!(stale.code, ControlErrorCode::StaleTarget);
    }

    #[test]
    fn mock_enforces_timeout_and_download_policy_without_writing_files() {
        let mut delayed = MockBrowserFixture::default();
        delayed.operation_delay = Duration::from_millis(10);
        let mut registry = BrowserSurfaceRegistry::with_mock_backend(delayed);
        let info = registry
            .create_surface(
                1,
                BrowserSurfaceOptions {
                    backend: BrowserBackendPreference::Mock,
                    ..Default::default()
                },
            )
            .unwrap();

        let timed_out = registry
            .handle_control_command(
                ControlCommand::BrowserEvaluateJavaScript {
                    workspace_id: 1,
                    surface_id: info.surface.id,
                    script: "document.title".to_string(),
                },
                Duration::from_millis(1),
            )
            .unwrap()
            .unwrap_err();
        assert_eq!(timed_out.code, ControlErrorCode::Timeout);
        assert!(timed_out.retryable);

        let denied = registry
            .handle_control_command(
                ControlCommand::BrowserDownload {
                    workspace_id: 1,
                    surface_id: info.surface.id,
                    url: "https://example.test/report.csv".to_string(),
                    suggested_filename: None,
                },
                Duration::from_millis(100),
            )
            .unwrap()
            .unwrap_err();
        assert_eq!(denied.code, ControlErrorCode::DownloadDenied);

        let mut allowed = BrowserSurfaceOptions {
            backend: BrowserBackendPreference::Mock,
            ..Default::default()
        };
        allowed.downloads = BrowserDownloadPolicy::AllowTo {
            directory: "/sandbox/downloads".to_string(),
        };
        let allowed_info = registry.create_surface(2, allowed).unwrap();
        let download = registry
            .handle_control_command(
                ControlCommand::BrowserDownload {
                    workspace_id: 2,
                    surface_id: allowed_info.surface.id,
                    url: "https://example.test/report.csv".to_string(),
                    suggested_filename: Some("../report.csv".to_string()),
                },
                Duration::from_millis(100),
            )
            .unwrap()
            .unwrap();
        assert!(matches!(
            download,
            ControlResult::BrowserDownload(BrowserDownloadResult {
                state: BrowserDownloadState::Accepted,
                ..
            })
        ));
    }

    #[test]
    fn control_frame_round_trip_uses_typed_browser_errors() {
        let mut handler = RegistryHandler {
            registry: BrowserSurfaceRegistry::with_mock_backend(MockBrowserFixture::default()),
        };
        let create = browser_request(
            1,
            ControlCommand::SurfaceCreateBrowser {
                workspace_id: 3,
                options: BrowserSurfaceOptions {
                    backend: BrowserBackendPreference::Mock,
                    ..Default::default()
                },
            },
        );
        let create = dispatch_frame(&mut handler, &serde_json::to_vec(&create).unwrap());
        let surface_id = match create {
            crate::control::ControlResponse::Ok {
                result: ControlResult::BrowserSurface(info),
                ..
            } => info.surface.id,
            response => panic!("expected browser create response, got {response:?}"),
        };

        let request = browser_request(
            2,
            ControlCommand::BrowserInteract {
                workspace_id: 3,
                surface_id,
                target: BrowserTarget::Css {
                    document_id: "wrong-document".to_string(),
                    selector: "#submit".to_string(),
                },
                action: BrowserDomAction::Click,
            },
        );
        let response = dispatch_frame(&mut handler, &serde_json::to_vec(&request).unwrap());
        match response {
            crate::control::ControlResponse::Error { error, .. } => {
                assert_eq!(error.code, ControlErrorCode::StaleTarget);
                assert!(!error.retryable);
            }
            response => panic!("expected typed stale-target response, got {response:?}"),
        }
    }
}
