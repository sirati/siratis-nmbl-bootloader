#[cfg(feature = "image-splash")]
use std::path::PathBuf;

#[cfg(feature = "image-splash")]
use serde::Deserialize;

/// Where the splash background PNG lives. Mirrors [`RescueMode`]'s
/// naming/shape: `"initrd"` (the default) keeps today's embedded
/// behaviour — the background is baked into the initramfs at
/// [`Splash::background_image`]; `"boot-partition"` reads the PNG from
/// the mounted boot partition at runtime (a sidecar next to the
/// initrd), resolved against [`Config::runtime_boot_mountpoint`] the
/// same way `rescue::locate_sfs` resolves `nmbl-rescue.sfs`. Persists
/// to TOML as kebab-case strings (`"initrd"`, `"boot-partition"`).
#[cfg(feature = "image-splash")]
#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SplashBackgroundLocation {
    /// Legacy: the background PNG is embedded in the initramfs at
    /// [`Splash::background_image`]. Loaded directly from that path.
    #[default]
    Initrd,
    /// Sidecar: the background PNG is staged on the boot partition next
    /// to the initrd and read at runtime, resolved against the runtime
    /// boot mountpoint (Phase 0.5). Falls back to a solid background if
    /// missing/unreadable.
    BootPartition,
}

#[cfg(feature = "image-splash")]
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Splash {
    #[serde(default)]
    pub enable: bool,

    #[serde(default = "default_splash_background")]
    pub background_image: PathBuf,

    /// Selects where the background PNG lives. Defaults to
    /// [`SplashBackgroundLocation::Initrd`] so configs predating this
    /// knob keep the embedded behaviour. In
    /// [`SplashBackgroundLocation::BootPartition`] mode the PNG is read
    /// from a FIXED basename next to the initrd on the boot partition
    /// (see `crate::ui::console::splash::SIDECAR_SPLASH_BG_BASENAME`);
    /// the name is intentionally not configurable.
    #[serde(default)]
    pub background_location: SplashBackgroundLocation,

    #[serde(default = "default_splash_font")]
    pub font_path: PathBuf,

    #[serde(default = "default_dri_path")]
    pub dri_path: PathBuf,
}

#[cfg(feature = "image-splash")]
impl Default for Splash {
    fn default() -> Self {
        Self {
            enable: false,
            background_image: default_splash_background(),
            background_location: SplashBackgroundLocation::default(),
            font_path: default_splash_font(),
            dri_path: default_dri_path(),
        }
    }
}

#[cfg(feature = "image-splash")]
fn default_splash_background() -> PathBuf {
    PathBuf::from("/etc/splash/image.png")
}

#[cfg(feature = "image-splash")]
fn default_splash_font() -> PathBuf {
    PathBuf::from("/etc/splash/font.ttf")
}

#[cfg(feature = "image-splash")]
fn default_dri_path() -> PathBuf {
    PathBuf::from("/dev/dri/card0")
}
