mod event_coalescer;

use crate::TelemetrySettings;
use anyhow::Result;
use clock::SystemClock;
use futures::channel::mpsc;
use futures::{Future, FutureExt, StreamExt};
use gpui::{App, AppContext as _, BackgroundExecutor, Task};
use http_client::{self, AsyncBody, HttpClient, HttpClientWithUrl, Method, Request};
use parking_lot::Mutex;
use regex::Regex;
use release_channel::ReleaseChannel;
use settings::{Settings, SettingsStore};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::fs::File;
use std::io::Write;
use std::sync::LazyLock;
use std::time::Instant;
use std::{env, mem, path::PathBuf, sync::Arc, time::Duration};
use telemetry_events::{AssistantEventData, AssistantPhase, Event, EventRequestBody, EventWrapper};
use util::{ResultExt, TryFutureExt};
use worktree::{UpdatedEntriesSet, WorktreeId};

use self::event_coalescer::EventCoalescer;

pub struct Telemetry {
    clock: Arc<dyn SystemClock>,
    http_client: Arc<HttpClientWithUrl>,
    executor: BackgroundExecutor,
    state: Arc<Mutex<TelemetryState>>,
}

struct TelemetryState {
    settings: TelemetrySettings,
    system_id: Option<Arc<str>>,       // Per system
    installation_id: Option<Arc<str>>, // Per app installation (different for dev, nightly, preview, and stable)
    session_id: Option<String>,        // Per app launch
    metrics_id: Option<Arc<str>>,      // Per logged-in user
    release_channel: Option<&'static str>,
    architecture: &'static str,
    events_queue: Vec<EventWrapper>,
    flush_events_task: Option<Task<()>>,
    log_file: Option<File>,
    is_staff: Option<bool>,
    first_event_date_time: Option<Instant>,
    event_coalescer: EventCoalescer,
    max_queue_size: usize,
    worktrees_with_project_type_events_sent: HashSet<WorktreeId>,

    os_name: String,
    app_version: String,
    os_version: Option<String>,
}

#[cfg(debug_assertions)]
const MAX_QUEUE_LEN: usize = 5;

#[cfg(not(debug_assertions))]
const MAX_QUEUE_LEN: usize = 50;

#[cfg(debug_assertions)]
const FLUSH_INTERVAL: Duration = Duration::from_secs(1);

#[cfg(not(debug_assertions))]
const FLUSH_INTERVAL: Duration = Duration::from_secs(60 * 5);
static ZED_CLIENT_CHECKSUM_SEED: LazyLock<Option<Vec<u8>>> = LazyLock::new(|| {
    option_env!("ZED_CLIENT_CHECKSUM_SEED")
        .map(|s| s.as_bytes().into())
        .or_else(|| {
            env::var("ZED_CLIENT_CHECKSUM_SEED")
                .ok()
                .map(|s| s.as_bytes().into())
        })
});

pub static MINIDUMP_ENDPOINT: LazyLock<Option<String>> = LazyLock::new(|| {
    option_env!("ZED_MINIDUMP_ENDPOINT")
        .map(|s| s.to_owned())
        .or_else(|| env::var("ZED_MINIDUMP_ENDPOINT").ok())
});

static DOTNET_PROJECT_FILES_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(global\.json|Directory\.Build\.props|.*\.(csproj|fsproj|vbproj|sln))$").unwrap()
});

pub fn os_name() -> String {
    #[cfg(target_os = "macos")]
    {
        "macOS".to_string()
    }
    #[cfg(target_os = "linux")]
    {
        format!("Linux {}", gpui::guess_compositor())
    }
    #[cfg(target_os = "freebsd")]
    {
        format!("FreeBSD {}", gpui::guess_compositor())
    }

    #[cfg(target_os = "windows")]
    {
        "Windows".to_string()
    }
}

/// Note: This might do blocking IO! Only call from background threads
pub fn os_version() -> String {
    #[cfg(target_os = "macos")]
    {
        use cocoa::base::nil;
        use cocoa::foundation::NSProcessInfo;

        unsafe {
            let process_info = cocoa::foundation::NSProcessInfo::processInfo(nil);
            let version = process_info.operatingSystemVersion();
            gpui::SemanticVersion::new(
                version.majorVersion as usize,
                version.minorVersion as usize,
                version.patchVersion as usize,
            )
            .to_string()
        }
    }
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    {
        use std::path::Path;

        let content = if let Ok(file) = std::fs::read_to_string(&Path::new("/etc/os-release")) {
            file
        } else if let Ok(file) = std::fs::read_to_string(&Path::new("/usr/lib/os-release")) {
            file
        } else if let Ok(file) = std::fs::read_to_string(&Path::new("/var/run/os-release")) {
            file
        } else {
            log::error!(
                "Failed to load /etc/os-release, /usr/lib/os-release, or /var/run/os-release"
            );
            "".to_string()
        };
        let mut name = "unknown";
        let mut version = "unknown";

        for line in content.lines() {
            match line.split_once('=') {
                Some(("ID", val)) => name = val.trim_matches('"'),
                Some(("VERSION_ID", val)) => version = val.trim_matches('"'),
                _ => {}
            }
        }

        format!("{} {}", name, version)
    }

    #[cfg(target_os = "windows")]
    {
        let mut info = unsafe { std::mem::zeroed() };
        let status = unsafe { windows::Wdk::System::SystemServices::RtlGetVersion(&mut info) };
        if status.is_ok() {
            gpui::SemanticVersion::new(
                info.dwMajorVersion as _,
                info.dwMinorVersion as _,
                info.dwBuildNumber as _,
            )
            .to_string()
        } else {
            "unknown".to_string()
        }
    }
}

impl Telemetry {
    pub fn new(
        clock: Arc<dyn SystemClock>,
        client: Arc<HttpClientWithUrl>,
        cx: &mut App,
    ) -> Arc<Self> {
        let release_channel =
            ReleaseChannel::try_global(cx).map(|release_channel| release_channel.display_name());

        TelemetrySettings::register(cx);

        let state = Arc::new(Mutex::new(TelemetryState {
            // Fred always disables telemetry settings here
            settings: TelemetrySettings {
                diagnostics: false,
                metrics: false,
            },
            architecture: env::consts::ARCH,
            release_channel,
            system_id: None,
            installation_id: None,
            session_id: None,
            metrics_id: None,
            events_queue: Vec::new(),
            flush_events_task: None,
            log_file: None,
            is_staff: None,
            first_event_date_time: None,
            event_coalescer: EventCoalescer::new(clock.clone()),
            max_queue_size: MAX_QUEUE_LEN,
            worktrees_with_project_type_events_sent: HashSet::new(),

            os_version: None,
            os_name: os_name(),
            app_version: release_channel::AppVersion::global(cx).to_string(),
        }));
        Self::log_file_path();

        let this = Arc::new(Self {
            clock,
            http_client: client,
            executor: cx.background_executor().clone(),
            state,
        });

        let (tx, mut rx) = mpsc::unbounded();
        ::telemetry::init(tx);

        cx.background_spawn({
            let this = Arc::downgrade(&this);
            async move {
                while let Some(event) = rx.next().await {
                    let Some(state) = this.upgrade() else { break };
                    state.report_event(Event::Flexible(event))
                }
            }
        })
        .detach();

        // We should only ever have one instance of Telemetry, leak the subscription to keep it alive
        // rather than store in TelemetryState, complicating spawn as subscriptions are not Send
        std::mem::forget(cx.on_app_quit({
            let this = this.clone();
            move |_| this.shutdown_telemetry()
        }));

        this
    }

    #[cfg(any(test, feature = "test-support"))]
    fn shutdown_telemetry(self: &Arc<Self>) -> impl Future<Output = ()> + use<> {
        Task::ready(())
    }

    // Skip calling this function in tests.
    // TestAppContext ends up calling this function on shutdown and it panics when trying to find the TelemetrySettings
    #[cfg(not(any(test, feature = "test-support")))]
    fn shutdown_telemetry(self: &Arc<Self>) -> impl Future<Output = ()> + use<> {
        telemetry::event!("App Closed");
        // TODO: close final edit period and make sure it's sent
        Task::ready(())
    }

    pub fn log_file_path() -> PathBuf {
        paths::logs_dir().join("telemetry.log")
    }

    pub fn has_checksum_seed(&self) -> bool {
        ZED_CLIENT_CHECKSUM_SEED.is_some()
    }

    pub fn start(
        self: &Arc<Self>,
        system_id: Option<String>,
        installation_id: Option<String>,
        session_id: String,
        cx: &App,
    ) {
        let mut state = self.state.lock();
        state.system_id = system_id.map(|id| id.into());
        state.installation_id = installation_id.map(|id| id.into());
        state.session_id = Some(session_id);
        state.app_version = release_channel::AppVersion::global(cx).to_string();
        state.os_name = os_name();
    }

    pub fn metrics_enabled(self: &Arc<Self>) -> bool {
        // Fred does not enable metrics
        false
    }

    pub fn set_authenticated_user_info(
        self: &Arc<Self>,
        metrics_id: Option<String>,
        is_staff: bool,
    ) {
        let mut state = self.state.lock();

        if !state.settings.metrics {
            return;
        }

        let metrics_id: Option<Arc<str>> = metrics_id.map(|id| id.into());
        state.metrics_id.clone_from(&metrics_id);
        state.is_staff = Some(is_staff);
        drop(state);
    }

    pub fn report_assistant_event(self: &Arc<Self>, event: AssistantEventData) {
        let event_type = match event.phase {
            AssistantPhase::Response => "Assistant Responded",
            AssistantPhase::Invoked => "Assistant Invoked",
            AssistantPhase::Accepted => "Assistant Response Accepted",
            AssistantPhase::Rejected => "Assistant Response Rejected",
        };

        telemetry::event!(
            event_type,
            conversation_id = event.conversation_id,
            kind = event.kind,
            phase = event.phase,
            message_id = event.message_id,
            model = event.model,
            model_provider = event.model_provider,
            response_latency = event.response_latency,
            error_message = event.error_message,
            language_name = event.language_name,
        );
    }

    pub fn log_edit_event(self: &Arc<Self>, environment: &'static str, is_via_ssh: bool) {
        let mut state = self.state.lock();
        let period_data = state.event_coalescer.log_event(environment);
        drop(state);

        if let Some((start, end, environment)) = period_data {
            let duration = end
                .saturating_duration_since(start)
                .min(Duration::from_secs(60 * 60 * 24))
                .as_millis() as i64;

            telemetry::event!(
                "Editor Edited",
                duration = duration,
                environment = environment,
                is_via_ssh = is_via_ssh
            );
        }
    }

    pub fn report_discovered_project_type_events(
        self: &Arc<Self>,
        worktree_id: WorktreeId,
        updated_entries_set: &UpdatedEntriesSet,
    ) {
        let Some(project_types) = self.detect_project_types(worktree_id, updated_entries_set)
        else {
            return;
        };

        for project_type in project_types {
            telemetry::event!("Project Opened", project_type = project_type);
        }
    }

    fn detect_project_types(
        self: &Arc<Self>,
        worktree_id: WorktreeId,
        updated_entries_set: &UpdatedEntriesSet,
    ) -> Option<Vec<String>> {
        let mut state = self.state.lock();

        if state
            .worktrees_with_project_type_events_sent
            .contains(&worktree_id)
        {
            return None;
        }

        let mut project_types: HashSet<&str> = HashSet::new();

        for (path, _, _) in updated_entries_set.iter() {
            let Some(file_name) = path.file_name().and_then(|f| f.to_str()) else {
                continue;
            };

            let project_type = if file_name == "pnpm-lock.yaml" {
                Some("pnpm")
            } else if file_name == "yarn.lock" {
                Some("yarn")
            } else if file_name == "package.json" {
                Some("node")
            } else if DOTNET_PROJECT_FILES_REGEX.is_match(file_name) {
                Some("dotnet")
            } else {
                None
            };

            if let Some(project_type) = project_type {
                project_types.insert(project_type);
            };
        }

        if !project_types.is_empty() {
            state
                .worktrees_with_project_type_events_sent
                .insert(worktree_id);
        }

        let mut project_types: Vec<_> = project_types.into_iter().map(String::from).collect();
        project_types.sort();
        Some(project_types)
    }

    fn report_event(self: &Arc<Self>, event: Event) {
        // Fred does not do telemetry
        return;
    }

    pub fn metrics_id(self: &Arc<Self>) -> Option<Arc<str>> {
        self.state.lock().metrics_id.clone()
    }

    pub fn system_id(self: &Arc<Self>) -> Option<Arc<str>> {
        self.state.lock().system_id.clone()
    }

    pub fn installation_id(self: &Arc<Self>) -> Option<Arc<str>> {
        self.state.lock().installation_id.clone()
    }

    pub fn is_staff(self: &Arc<Self>) -> Option<bool> {
        self.state.lock().is_staff
    }

    fn build_request(
        self: &Arc<Self>,
        // We take in the JSON bytes buffer so we can reuse the existing allocation.
        mut json_bytes: Vec<u8>,
        event_request: &EventRequestBody,
    ) -> Result<Request<AsyncBody>> {
        json_bytes.clear();
        serde_json::to_writer(&mut json_bytes, event_request)?;

        let checksum = calculate_json_checksum(&json_bytes).unwrap_or_default();

        Ok(Request::builder()
            .method(Method::POST)
            .uri(
                self.http_client
                    .build_zed_api_url("/telemetry/events", &[])?
                    .as_ref(),
            )
            .header("Content-Type", "application/json")
            .header("x-zed-checksum", checksum)
            .body(json_bytes.into())?)
    }

    pub fn flush_events(self: &Arc<Self>) -> Task<()> {
        // Fred does not do telemetry
        let mut state = self.state.lock();
        state.events_queue.clear();
        return Task::ready(());
    }
}

pub fn calculate_json_checksum(json: &impl AsRef<[u8]>) -> Option<String> {
    let Some(checksum_seed) = &*ZED_CLIENT_CHECKSUM_SEED else {
        return None;
    };

    let mut summer = Sha256::new();
    summer.update(checksum_seed);
    summer.update(json);
    summer.update(checksum_seed);
    let mut checksum = String::new();
    for byte in summer.finalize().as_slice() {
        use std::fmt::Write;
        write!(&mut checksum, "{:02x}", byte).unwrap();
    }

    Some(checksum)
}
