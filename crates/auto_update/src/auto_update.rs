use anyhow::{Context as _, Result, anyhow, bail};
use client::{Client, TelemetrySettings};
use db::RELEASE_CHANNEL;
use db::kvp::KEY_VALUE_STORE;
use gpui::{
    App, AppContext as _, AsyncApp, Context, Entity, Global, SemanticVersion, Task, Window, actions,
};
use http_client::{AsyncBody, HttpClient, HttpClientWithUrl};
use paths::remote_servers_dir;
use release_channel::{AppCommitSha, ReleaseChannel};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use settings::{Settings, SettingsSources, SettingsStore};
use smol::{fs, io::AsyncReadExt};
use smol::{fs::File, process::Command};
use std::{
    env::{
        self,
        consts::{ARCH, OS},
    },
    ffi::OsString,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use workspace::Workspace;

const SHOULD_SHOW_UPDATE_NOTIFICATION_KEY: &str = "auto-updater-should-show-updated-notification";

actions!(auto_update, [Check, DismissErrorMessage, ViewReleaseNotes,]);

#[derive(Clone, PartialEq, Eq)]
pub enum AutoUpdateStatus {
    Idle,
    Checking,
    Downloading,
    Installing,
    Updated { binary_path: PathBuf },
    Errored,
}

impl AutoUpdateStatus {
    pub fn is_updated(&self) -> bool {
        matches!(self, Self::Updated { .. })
    }
}

pub struct AutoUpdater {
    status: AutoUpdateStatus,
    current_version: SemanticVersion,
    http_client: Arc<HttpClientWithUrl>,
    pending_poll: Option<Task<Option<()>>>,
}

#[derive(Deserialize, Debug)]
pub struct JsonRelease {
    pub version: String,
    pub url: String,
}

struct MacOsUnmounter {
    mount_path: PathBuf,
}

impl Drop for MacOsUnmounter {
    fn drop(&mut self) {
        let unmount_output = std::process::Command::new("hdiutil")
            .args(["detach", "-force"])
            .arg(&self.mount_path)
            .output();

        match unmount_output {
            Ok(output) if output.status.success() => {
                log::info!("Successfully unmounted the disk image");
            }
            Ok(output) => {
                log::error!(
                    "Failed to unmount disk image: {:?}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            Err(error) => {
                log::error!("Error while trying to unmount disk image: {:?}", error);
            }
        }
    }
}

struct AutoUpdateSetting(bool);

/// Whether or not to automatically check for updates.
///
/// Default: true
#[derive(Clone, Copy, Default, JsonSchema, Deserialize, Serialize)]
#[serde(transparent)]
struct AutoUpdateSettingContent(bool);

impl Settings for AutoUpdateSetting {
    const KEY: Option<&'static str> = Some("auto_update");

    type FileContent = Option<AutoUpdateSettingContent>;

    fn load(sources: SettingsSources<Self::FileContent>, _: &mut App) -> Result<Self> {
        let auto_update = [sources.server, sources.release_channel, sources.user]
            .into_iter()
            .find_map(|value| value.copied().flatten())
            .unwrap_or(sources.default.ok_or_else(Self::missing_default)?);

        Ok(Self(auto_update.0))
    }

    fn import_from_vscode(vscode: &settings::VsCodeSettings, current: &mut Self::FileContent) {
        vscode.enum_setting("update.mode", current, |s| match s {
            "none" | "manual" => Some(AutoUpdateSettingContent(false)),
            _ => Some(AutoUpdateSettingContent(true)),
        });
    }
}

#[derive(Default)]
struct GlobalAutoUpdate(Option<Entity<AutoUpdater>>);

impl Global for GlobalAutoUpdate {}

pub fn init(http_client: Arc<HttpClientWithUrl>, cx: &mut App) {
    // Fred does not auto-update
}

pub fn check(_: &Check, window: &mut Window, cx: &mut App) {
    drop(window.prompt(
        gpui::PromptLevel::Info,
        "Fred does not auto-update",
        None,
        &["Ok"],
        cx,
    ));
}

pub fn view_release_notes(_: &ViewReleaseNotes, cx: &mut App) -> Option<()> {
    let auto_updater = AutoUpdater::get(cx)?;
    let release_channel = ReleaseChannel::try_global(cx)?;

    match release_channel {
        ReleaseChannel::Stable | ReleaseChannel::Preview => {
            let auto_updater = auto_updater.read(cx);
            let current_version = auto_updater.current_version;
            let release_channel = release_channel.dev_name();
            let path = format!("/releases/{release_channel}/{current_version}");
            let url = &auto_updater.http_client.build_url(&path);
            cx.open_url(url);
        }
        ReleaseChannel::Nightly => {
            cx.open_url("https://github.com/zed-industries/zed/commits/nightly/");
        }
        ReleaseChannel::Dev => {
            cx.open_url("https://github.com/zed-industries/zed/commits/main/");
        }
    }
    None
}

impl AutoUpdater {
    pub fn get(cx: &mut App) -> Option<Entity<Self>> {
        cx.default_global::<GlobalAutoUpdate>().0.clone()
    }

    fn new(current_version: SemanticVersion, http_client: Arc<HttpClientWithUrl>) -> Self {
        Self {
            status: AutoUpdateStatus::Idle,
            current_version,
            http_client,
            pending_poll: None,
        }
    }

    pub fn current_version(&self) -> SemanticVersion {
        self.current_version
    }

    pub fn status(&self) -> AutoUpdateStatus {
        self.status.clone()
    }

    pub fn dismiss_error(&mut self, cx: &mut Context<Self>) {
        self.status = AutoUpdateStatus::Idle;
        cx.notify();
    }

    // If you are packaging Zed and need to override the place it downloads SSH remotes from,
    // you can override this function. You should also update get_remote_server_release_url to return
    // Ok(None).
    pub async fn download_remote_server_release(
        os: &str,
        arch: &str,
        release_channel: ReleaseChannel,
        version: Option<SemanticVersion>,
        cx: &mut AsyncApp,
    ) -> Result<PathBuf> {
        bail!("Fred does not download remote server binaries")
    }

    pub async fn get_remote_server_release_url(
        os: &str,
        arch: &str,
        release_channel: ReleaseChannel,
        version: Option<SemanticVersion>,
        cx: &mut AsyncApp,
    ) -> Result<Option<(String, String)>> {
        // ???
        Ok(None)
    }

    pub fn set_should_show_update_notification(
        &self,
        should_show: bool,
        cx: &App,
    ) -> Task<Result<()>> {
        cx.background_spawn(async move {
            if should_show {
                KEY_VALUE_STORE
                    .write_kvp(
                        SHOULD_SHOW_UPDATE_NOTIFICATION_KEY.to_string(),
                        "".to_string(),
                    )
                    .await?;
            } else {
                KEY_VALUE_STORE
                    .delete_kvp(SHOULD_SHOW_UPDATE_NOTIFICATION_KEY.to_string())
                    .await?;
            }
            Ok(())
        })
    }

    pub fn should_show_update_notification(&self, cx: &App) -> Task<Result<bool>> {
        cx.background_spawn(async move {
            Ok(KEY_VALUE_STORE
                .read_kvp(SHOULD_SHOW_UPDATE_NOTIFICATION_KEY)?
                .is_some())
        })
    }
}

pub fn check_pending_installation() -> bool {
    let Some(installer_path) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.join("updates")))
    else {
        return false;
    };

    // The installer will create a flag file after it finishes updating
    let flag_file = installer_path.join("versions.txt");
    if flag_file.exists() {
        if let Some(helper) = installer_path
            .parent()
            .map(|p| p.join("tools\\auto_update_helper.exe"))
        {
            let _ = std::process::Command::new(helper).spawn();
            return true;
        }
    }
    false
}
