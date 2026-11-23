//! Main-thread Sparkle host. Sparkle's installer requests ordinary AppKit termination,
//! which the desktop app routes through the same cancelable close as the window button.

use objc2_foundation::{NSBundle, NSString};
use sparkle_updater::{MainThreadMarker, SparkleUpdater, UpdaterConfig};

fn initialize() -> Result<SparkleUpdater, String> {
    let feed = option_env!("CODESIGN_UPDATE_FEED").ok_or_else(|| "This build has no update feed.".to_string())?;
    let bundle = NSBundle::mainBundle();
    if bundle.bundleIdentifier().is_none_or(|identifier| identifier.to_string() != "com.larshoie.codesign") {
        return Err("Install the Collate application bundle to check for updates.".into());
    }
    let bundled_feed = bundle.objectForInfoDictionaryKey(&NSString::from_str("SUFeedURL")).and_then(|value| value.downcast::<NSString>().ok());
    if bundled_feed.is_none_or(|url| url.to_string() != feed) {
        return Err("The packaged appcast URL does not match this build.".into());
    }
    let marker = MainThreadMarker::new().ok_or_else(|| "Initialize the updater on the main thread.".to_string())?;
    let updater = SparkleUpdater::new(marker, UpdaterConfig::default())
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "Install the application bundle to check for updates.".to_string())?;
    // The initial check runs on the next AppKit cycle. Pin the release feed before it runs.
    updater.clear_feed_url_from_user_defaults().map_err(|error| error.to_string())?;
    updater.set_feed_url_override(Some(feed.to_owned())).map_err(|error| error.to_string())?;
    if option_env!("CODESIGN_UPDATE_CHANNEL") == Some("beta") {
        updater.set_allowed_channels(Some(vec!["beta".into()])).map_err(|error| error.to_string())?;
    }
    Ok(updater)
}

pub fn start() -> Option<impl Fn() -> Result<(), String>> {
    match initialize() {
        Ok(updater) => Some(move || updater.check_for_updates().map_err(|error| error.to_string())),
        Err(error) => {
            log::warn!("macOS updater unavailable: {error}");
            None
        }
    }
}

/// Packaging smoke test. Confirm the linked framework and bundled application agree on the feed.
pub fn check_configuration() -> std::process::ExitCode {
    if let Some(marker) = MainThreadMarker::new() {
        let _application = objc2_app_kit::NSApplication::sharedApplication(marker);
    }
    let result = initialize().and_then(|updater| {
        let feed = updater.feed_url().map_err(|error| error.to_string())?;
        if feed.as_deref() != option_env!("CODESIGN_UPDATE_FEED") {
            return Err("The updater is using an unexpected feed.".into());
        }
        if !updater.can_check_for_updates().map_err(|error| error.to_string())? {
            return Err("The updater is unable to check for updates.".into());
        }
        Ok(())
    });
    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("Collate updater: {error}");
            std::process::ExitCode::FAILURE
        }
    }
}
