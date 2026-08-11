//! Settings governance: the corpus of known macOS settings, the
//! desired-state manifest, and the readers/writers that keep wukong
//! coherent with `cfprefsd`.
//!
//! The cardinal rule: READ preference plists directly (fast, no
//! forks — the `plist` crate handles binary and XML alike), but WRITE
//! only through the `defaults` CLI, which goes through `cfprefsd`.
//! Writing plist files directly desynchronizes the preferences daemon
//! and the change may be overwritten or ignored.
//!
//! The corpus is curated knowledge: domain, key, a human label, and
//! which process must restart for the change to take effect. Values
//! are NOT part of the corpus — desired values live in the manifest,
//! chosen by the user. Settings outside the corpus are governable too,
//! via explicit `wukong settings record`.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Where the settings manifest lives inside the store repo.
pub const MANIFEST_REL: &str = "__wukong__/settings.toml";

/// One curated setting: identity and apply-knowledge, no value.
#[derive(Debug, Clone, Copy)]
pub struct Known {
    pub domain: &'static str,
    pub key: &'static str,
    pub label: &'static str,
    pub group: &'static str,
    /// Process to `killall` after applying, if any.
    pub restart: Option<&'static str>,
}

const fn k(
    domain: &'static str,
    key: &'static str,
    label: &'static str,
    group: &'static str,
    restart: Option<&'static str>,
) -> Known {
    Known {
        domain,
        key,
        label,
        group,
        restart,
    }
}

/// The curated corpus, grouped for display.
pub const CORPUS: &[Known] = &[
    k(
        "NSGlobalDomain",
        "AppleReduceDesktopTinting",
        "Stop windows picking up a tint from the wallpaper",
        "Desktop & Windows",
        Some("Dock"),
    ),
    k(
        "NSGlobalDomain",
        "AppleSpacesSwitchOnActivate",
        "Switch to an app without being thrown to another desktop",
        "Desktop & Windows",
        Some("Dock"),
    ),
    k(
        "NSGlobalDomain",
        "com.apple.swipescrolldirection",
        "Scroll the way a mouse wheel expects",
        "Desktop & Windows",
        None,
    ),
    k(
        "com.apple.AppleMultitouchTrackpad",
        "Clicking",
        "Tap the trackpad instead of pressing it down",
        "Desktop & Windows",
        None,
    ),
    k(
        "com.apple.AppleMultitouchTrackpad",
        "TrackpadThreeFingerDrag",
        "Move a window by dragging three fingers on it",
        "Desktop & Windows",
        None,
    ),
    k(
        "com.apple.WindowManager",
        "DisableTilingAnimations",
        "Tile windows without the animation",
        "Desktop & Windows",
        Some("WindowManager"),
    ),
    k(
        "com.apple.WindowManager",
        "EnableStandardClickToShowDesktop",
        "Click the wallpaper without your windows sliding away",
        "Desktop & Windows",
        Some("WindowManager"),
    ),
    k(
        "com.apple.WindowManager",
        "EnableTiledWindowMargins",
        "Tile windows with no gap between them",
        "Desktop & Windows",
        Some("WindowManager"),
    ),
    k(
        "com.apple.WindowManager",
        "EnableTilingByEdgeDrag",
        "Drag a window to a screen edge without it tiling",
        "Desktop & Windows",
        Some("WindowManager"),
    ),
    k(
        "com.apple.WindowManager",
        "EnableTopTilingByEdgeDrag",
        "Drag a window to the menu bar without it going full screen",
        "Desktop & Windows",
        Some("WindowManager"),
    ),
    k(
        "com.apple.finder",
        "CreateDesktop",
        "Keep the desktop free of icons",
        "Desktop & Windows",
        Some("Finder"),
    ),
    k(
        "NSGlobalDomain",
        "WebKitDeveloperExtras",
        "Add Inspect Element to web views inside other apps",
        "Developer",
        None,
    ),
    k(
        "com.apple.ActivityMonitor",
        "IconType",
        "Draw a CPU history graph on the Activity Monitor icon",
        "Developer",
        Some("Activity Monitor"),
    ),
    k(
        "com.apple.AdLib",
        "allowApplePersonalizedAdvertising",
        "Turn off Apple's personalized advertising",
        "Developer",
        None,
    ),
    k(
        "com.apple.PowerChime",
        "ChimeOnNoHardware",
        "Stay quiet when the power cable goes in",
        "Developer",
        None,
    ),
    k(
        "com.apple.Terminal",
        "SecureKeyboardEntry",
        "Keep other apps from reading what you type in Terminal",
        "Developer",
        None,
    ),
    k(
        "com.apple.dt.Xcode",
        "DVTTextShowFoldingSidebar",
        "Show the code folding ribbon in Xcode",
        "Developer",
        None,
    ),
    k(
        "com.apple.dt.Xcode",
        "DVTTextShowLineNumbers",
        "Show line numbers in the Xcode editor",
        "Developer",
        None,
    ),
    k(
        "com.apple.dt.Xcode",
        "ShowBuildOperationDuration",
        "Show how long each build took in Xcode",
        "Developer",
        None,
    ),
    k(
        "com.apple.iphonesimulator",
        "ShowSingleTouches",
        "Show where you tap in the iOS Simulator",
        "Developer",
        None,
    ),
    k(
        "com.apple.dock",
        "appswitcher-all-displays",
        "Show the app switcher on every display",
        "Dock",
        Some("Dock"),
    ),
    k(
        "com.apple.dock",
        "autohide",
        "Hide the Dock until you point at it",
        "Dock",
        Some("Dock"),
    ),
    k(
        "com.apple.dock",
        "autohide-delay",
        "Show the hidden Dock without a pause",
        "Dock",
        Some("Dock"),
    ),
    k(
        "com.apple.dock",
        "autohide-time-modifier",
        "Slide the Dock in and out quickly",
        "Dock",
        Some("Dock"),
    ),
    k(
        "com.apple.dock",
        "enable-spring-load-actions-on-all-items",
        "Open a dragged file by holding it over any Dock icon",
        "Dock",
        Some("Dock"),
    ),
    k(
        "com.apple.dock",
        "expose-group-apps",
        "Group Mission Control windows by app",
        "Dock",
        Some("Dock"),
    ),
    k(
        "com.apple.dock",
        "launchanim",
        "Stop the Dock icon bouncing while an app opens",
        "Dock",
        Some("Dock"),
    ),
    k(
        "com.apple.dock",
        "mineffect",
        "Shrink windows straight down instead of the genie sweep",
        "Dock",
        Some("Dock"),
    ),
    k(
        "com.apple.dock",
        "minimize-to-application",
        "Minimize windows into their app icon",
        "Dock",
        Some("Dock"),
    ),
    k(
        "com.apple.dock",
        "mru-spaces",
        "Keep desktops in the order you put them",
        "Dock",
        Some("Dock"),
    ),
    k(
        "com.apple.dock",
        "scroll-to-open",
        "Scroll up on a Dock icon to see an app's windows",
        "Dock",
        Some("Dock"),
    ),
    k(
        "com.apple.dock",
        "show-recents",
        "Keep recent apps out of the Dock",
        "Dock",
        Some("Dock"),
    ),
    k(
        "com.apple.dock",
        "size-immutable",
        "Lock the Dock against an accidental resize",
        "Dock",
        Some("Dock"),
    ),
    k(
        "com.apple.dock",
        "static-only",
        "List only the apps that are open in the Dock",
        "Dock",
        Some("Dock"),
    ),
    k(
        "com.apple.dock",
        "tilesize",
        "Pin the Dock icons to 48 pixels",
        "Dock",
        Some("Dock"),
    ),
    k(
        "com.apple.dock",
        "wvous-br-corner",
        "Stop the bottom right corner opening a Quick Note",
        "Dock",
        Some("Dock"),
    ),
    k(
        "NSGlobalDomain",
        "AppleShowAllExtensions",
        "Show the extension on every file",
        "Finder",
        Some("Finder"),
    ),
    k(
        "NSGlobalDomain",
        "NSTableViewDefaultSizeMode",
        "Use small icons in the Finder sidebar",
        "Finder",
        Some("Finder"),
    ),
    k(
        "NSGlobalDomain",
        "NSToolbarTitleViewRolloverDelay",
        "Show the folder icon in a window title without a pause",
        "Finder",
        Some("Finder"),
    ),
    k(
        "NSGlobalDomain",
        "com.apple.springing.delay",
        "Spring a folder open sooner when you drag onto it",
        "Finder",
        None,
    ),
    k(
        "com.apple.NetworkBrowser",
        "BrowseAllInterfaces",
        "Let AirDrop work over a wired connection",
        "Finder",
        Some("sharingd"),
    ),
    k(
        "com.apple.desktopservices",
        "DSDontWriteNetworkStores",
        "Stop leaving .DS_Store files on network shares",
        "Finder",
        None,
    ),
    k(
        "com.apple.desktopservices",
        "DSDontWriteUSBStores",
        "Stop leaving .DS_Store files on USB drives",
        "Finder",
        None,
    ),
    k(
        "com.apple.finder",
        "AppleShowAllFiles",
        "Show hidden files in Finder",
        "Finder",
        Some("Finder"),
    ),
    k(
        "com.apple.finder",
        "DisableAllAnimations",
        "Turn off Finder's animations",
        "Finder",
        Some("Finder"),
    ),
    k(
        "com.apple.finder",
        "FXDefaultSearchScope",
        "Search the folder you are in, not the whole Mac",
        "Finder",
        Some("Finder"),
    ),
    k(
        "com.apple.finder",
        "FXEnableExtensionChangeWarning",
        "Stop asking before an extension changes",
        "Finder",
        Some("Finder"),
    ),
    k(
        "com.apple.finder",
        "FXPreferredViewStyle",
        "Open Finder windows as a list",
        "Finder",
        Some("Finder"),
    ),
    k(
        "com.apple.finder",
        "FXRemoveOldTrashItems",
        "Clear things out of the Trash after 30 days",
        "Finder",
        Some("Finder"),
    ),
    k(
        "com.apple.finder",
        "NewWindowTarget",
        "Open new Finder windows in your home folder",
        "Finder",
        Some("Finder"),
    ),
    k(
        "com.apple.finder",
        "ShowPathbar",
        "Show the folder path along the bottom of Finder windows",
        "Finder",
        Some("Finder"),
    ),
    k(
        "com.apple.finder",
        "ShowRecentTags",
        "Keep Recent Tags out of the Finder sidebar",
        "Finder",
        Some("Finder"),
    ),
    k(
        "com.apple.finder",
        "ShowStatusBar",
        "Show the item count and free space in Finder windows",
        "Finder",
        Some("Finder"),
    ),
    k(
        "com.apple.finder",
        "WarnOnEmptyTrash",
        "Empty the Trash without being asked first",
        "Finder",
        Some("Finder"),
    ),
    k(
        "com.apple.finder",
        "_FXEnableColumnAutoSizing",
        "Widen column view columns to fit the longest name",
        "Finder",
        Some("Finder"),
    ),
    k(
        "com.apple.finder",
        "_FXShowPosixPathInTitle",
        "Show the whole path in the Finder window title",
        "Finder",
        Some("Finder"),
    ),
    k(
        "com.apple.finder",
        "_FXSortFoldersFirst",
        "Sort folders above files",
        "Finder",
        Some("Finder"),
    ),
    k(
        "com.apple.finder",
        "_FXSortFoldersFirstOnDesktop",
        "Sort folders above files on the desktop",
        "Finder",
        Some("Finder"),
    ),
    k(
        "NSGlobalDomain",
        "AppleKeyboardUIMode",
        "Move focus to every control with Tab",
        "Keyboard & Text",
        None,
    ),
    k(
        "NSGlobalDomain",
        "ApplePressAndHoldEnabled",
        "Hold a letter to repeat it instead of picking an accent",
        "Keyboard & Text",
        None,
    ),
    k(
        "NSGlobalDomain",
        "InitialKeyRepeat",
        "Start repeating sooner when a key is held",
        "Keyboard & Text",
        None,
    ),
    k(
        "NSGlobalDomain",
        "KeyRepeat",
        "Repeat held keys faster than the settings slider allows",
        "Keyboard & Text",
        None,
    ),
    k(
        "NSGlobalDomain",
        "NSAutomaticCapitalizationEnabled",
        "Stop capitalizing the first word of a sentence",
        "Keyboard & Text",
        None,
    ),
    k(
        "NSGlobalDomain",
        "NSAutomaticDashSubstitutionEnabled",
        "Keep double hyphens as hyphens",
        "Keyboard & Text",
        None,
    ),
    k(
        "NSGlobalDomain",
        "NSAutomaticInlinePredictionEnabled",
        "Stop guessing the rest of the word as you type",
        "Keyboard & Text",
        None,
    ),
    k(
        "NSGlobalDomain",
        "NSAutomaticPeriodSubstitutionEnabled",
        "Stop turning a double space into a period",
        "Keyboard & Text",
        None,
    ),
    k(
        "NSGlobalDomain",
        "NSAutomaticQuoteSubstitutionEnabled",
        "Keep quote marks straight",
        "Keyboard & Text",
        None,
    ),
    k(
        "NSGlobalDomain",
        "NSAutomaticSpellingCorrectionEnabled",
        "Stop rewriting words as you type",
        "Keyboard & Text",
        None,
    ),
    k(
        "NSGlobalDomain",
        "NSTextShowsControlCharacters",
        "Show invisible control characters in text",
        "Keyboard & Text",
        None,
    ),
    k(
        "NSGlobalDomain",
        "com.apple.keyboard.fnState",
        "Use the top row as F1 through F12",
        "Keyboard & Text",
        None,
    ),
    k(
        "NSGlobalDomain",
        "AppleMenuBarVisibleInFullscreen",
        "Keep the menu bar on screen in full screen apps",
        "Menu Bar",
        None,
    ),
    k(
        "com.apple.menuextra.clock",
        "FlashDateSeparators",
        "Blink the colon in the menu bar clock",
        "Menu Bar",
        Some("ControlCenter"),
    ),
    k(
        "com.apple.menuextra.clock",
        "ShowDayOfWeek",
        "Show the day of the week in the menu bar",
        "Menu Bar",
        Some("ControlCenter"),
    ),
    k(
        "com.apple.menuextra.clock",
        "ShowSeconds",
        "Show seconds on the menu bar clock",
        "Menu Bar",
        Some("ControlCenter"),
    ),
    k(
        "com.apple.screencapture",
        "disable-shadow",
        "Leave the drop shadow off window screenshots",
        "Screenshots",
        Some("SystemUIServer"),
    ),
    k(
        "com.apple.screencapture",
        "show-thumbnail",
        "Skip the preview that floats after a screenshot",
        "Screenshots",
        Some("SystemUIServer"),
    ),
    k(
        "com.apple.screencapture",
        "type",
        "Save screenshots as PNG",
        "Screenshots",
        Some("SystemUIServer"),
    ),
    k(
        "NSGlobalDomain",
        "AppleScrollerPagingBehavior",
        "Click a scroll bar to jump to that spot",
        "Windows & Saving",
        None,
    ),
    k(
        "NSGlobalDomain",
        "AppleShowScrollBars",
        "Keep scroll bars on screen",
        "Windows & Saving",
        None,
    ),
    k(
        "NSGlobalDomain",
        "AppleWindowTabbingMode",
        "Open new windows as tabs",
        "Windows & Saving",
        None,
    ),
    k(
        "NSGlobalDomain",
        "NSAutomaticWindowAnimationsEnabled",
        "Open and close windows without the animation",
        "Windows & Saving",
        None,
    ),
    k(
        "NSGlobalDomain",
        "NSDocumentSaveNewDocumentsToCloud",
        "Save new documents to this Mac, not iCloud",
        "Windows & Saving",
        None,
    ),
    k(
        "NSGlobalDomain",
        "NSNavPanelExpandedStateForSaveMode",
        "Open Save dialogs in the full view",
        "Windows & Saving",
        None,
    ),
    k(
        "NSGlobalDomain",
        "NSQuitAlwaysKeepsWindows",
        "Reopen an app to a clean slate",
        "Windows & Saving",
        None,
    ),
    k(
        "NSGlobalDomain",
        "NSWindowResizeTime",
        "Resize windows without the animation",
        "Windows & Saving",
        None,
    ),
    k(
        "NSGlobalDomain",
        "NSWindowShouldDragOnGesture",
        "Move a window by dragging anywhere inside it",
        "Windows & Saving",
        None,
    ),
    k(
        "NSGlobalDomain",
        "PMPrintingExpandedStateForPrint2",
        "Open Print dialogs in the full view",
        "Windows & Saving",
        None,
    ),
    k(
        "com.apple.loginwindow",
        "TALLogoutSavesState",
        "Log back in to an empty desktop",
        "Windows & Saving",
        None,
    ),
];

/// Find corpus knowledge for a domain/key pair.
#[must_use]
pub fn known(domain: &str, key: &str) -> Option<&'static Known> {
    CORPUS.iter().find(|s| s.domain == domain && s.key == key)
}

/// A settings value — the scalar types `defaults` speaks. Arrays and
/// dictionaries are deliberately out of scope: every corpus setting is
/// scalar, and complex prefs are not worth governing blind.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Value {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    /// Any non-scalar defaults value — the Dock's `persistent-apps`
    /// array, `symbolichotkeys` dicts. Carried as plist XML; equality
    /// is structural, never textual. Complex keys are governed only
    /// when explicitly recorded — capture and ambient discovery stay
    /// scalar, because arrays and dicts are where app-state noise
    /// lives.
    Complex {
        plist: String,
    },
}

impl std::fmt::Display for Value {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Bool(v) => write!(f, "{v}"),
            Self::Int(v) => write!(f, "{v}"),
            Self::Float(v) => write!(f, "{v}"),
            Self::Str(v) => write!(f, "{v}"),
            Self::Complex { plist } => match parse_plist_xml(plist) {
                Some(plist::Value::Array(items)) => write!(f, "«array, {} item(s)»", items.len()),
                Some(plist::Value::Dictionary(d)) => write!(f, "«dict, {} key(s)»", d.len()),
                _ => write!(f, "«complex value»"),
            },
        }
    }
}

/// Structural parse of the XML we carry; `None` never panics a
/// display or comparison.
fn parse_plist_xml(xml: &str) -> Option<plist::Value> {
    plist::Value::from_reader(std::io::Cursor::new(xml.as_bytes())).ok()
}

/// The canonical XML form the manifest carries.
fn plist_to_xml(value: &plist::Value) -> Option<String> {
    let mut out = Vec::new();
    plist::to_writer_xml(&mut out, value).ok()?;
    String::from_utf8(out).ok()
}

impl Value {
    /// The `defaults write` argument tail. Scalars get a type flag;
    /// complex values ride as plist text, which `defaults` parses
    /// natively.
    #[must_use]
    pub fn defaults_args(&self) -> Vec<String> {
        match self {
            Self::Bool(v) => vec!["-bool".to_string(), v.to_string()],
            Self::Int(v) => vec!["-int".to_string(), v.to_string()],
            Self::Float(v) => vec!["-float".to_string(), v.to_string()],
            Self::Str(v) => vec!["-string".to_string(), v.clone()],
            Self::Complex { plist } => vec![plist.clone()],
        }
    }

    /// Semantic equality across the representations macOS actually
    /// uses: booleans round-trip as integers in some domains, and
    /// floats deserve an epsilon, not bit equality.
    #[must_use]
    pub fn matches(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Bool(a), Self::Bool(b)) => a == b,
            (Self::Int(a), Self::Int(b)) => a == b,
            (Self::Str(a), Self::Str(b)) => a == b,
            (Self::Bool(a), Self::Int(b)) | (Self::Int(b), Self::Bool(a)) => i64::from(*a) == *b,
            (Self::Float(a), Self::Float(b)) => (a - b).abs() < 1e-9,
            (Self::Float(a), Self::Int(b)) | (Self::Int(b), Self::Float(a)) => {
                #[allow(clippy::cast_precision_loss)] // pref ints are small
                let bf = *b as f64;
                (a - bf).abs() < 1e-9
            }
            (Self::Complex { plist: a }, Self::Complex { plist: b }) => {
                // Structural, never textual: two XML spellings of the
                // same array must match.
                match (parse_plist_xml(a), parse_plist_xml(b)) {
                    (Some(a), Some(b)) => a == b,
                    _ => a == b,
                }
            }
            _ => false,
        }
    }

    /// Convert from a plist value; `None` for the types we do not
    /// govern ambiently (capture and discovery stay scalar).
    #[must_use]
    pub fn from_plist(value: &plist::Value) -> Option<Self> {
        match value {
            plist::Value::Boolean(v) => Some(Self::Bool(*v)),
            plist::Value::Integer(v) => v.as_signed().map(Self::Int),
            plist::Value::Real(v) => Some(Self::Float(*v)),
            plist::Value::String(v) => Some(Self::Str(v.clone())),
            _ => None,
        }
    }

    /// Convert ANY plist value — the read path for governed keys,
    /// where an explicitly recorded array or dict is a first-class
    /// citizen.
    #[must_use]
    pub fn from_plist_any(value: &plist::Value) -> Option<Self> {
        Self::from_plist(value).or_else(|| plist_to_xml(value).map(|plist| Self::Complex { plist }))
    }
}

/// The process a domain's changes usually need restarted — restart
/// inference for keys outside the corpus, best-effort and overridable
/// at record time.
#[must_use]
pub fn restart_for_domain(domain: &str) -> Option<&'static str> {
    match domain {
        "com.apple.dock" | "com.apple.WindowManager" => Some("Dock"),
        "com.apple.finder" => Some("Finder"),
        "com.apple.SystemUIServer" | "com.apple.controlcenter" => Some("SystemUIServer"),
        _ => None,
    }
}

/// Desired state, synced through the store like everything else.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct SettingsManifest {
    /// domain → key → desired value.
    pub settings: BTreeMap<String, BTreeMap<String, Value>>,
    /// domain → keys never offered again.
    pub ignore: BTreeMap<String, BTreeSet<String>>,
    /// domain → key → process to restart after applying — restart
    /// knowledge for keys the corpus doesn't carry, recorded (or
    /// inferred from the domain) at record time.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub restarts: BTreeMap<String, BTreeMap<String, String>>,
    /// Forward compatibility across a mixed-version fleet: a newer
    /// wukong's fields survive this binary's round-trip.
    #[serde(flatten)]
    pub extra: BTreeMap<String, toml::Value>,
}

impl SettingsManifest {
    pub fn load(store_dir: &Path) -> Result<Option<Self>, String> {
        let path = store_dir.join(MANIFEST_REL);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("cannot read settings manifest: {e}")),
        };
        toml::from_str(&text)
            .map(Some)
            .map_err(|e| format!("settings manifest does not parse: {e}"))
    }

    pub fn save(&self, store_dir: &Path) -> std::io::Result<()> {
        let path = store_dir.join(MANIFEST_REL);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = toml::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, text)
    }

    #[must_use]
    pub fn desired(&self, domain: &str, key: &str) -> Option<&Value> {
        self.settings.get(domain)?.get(key)
    }

    pub fn set(&mut self, domain: &str, key: &str, value: Value) {
        self.settings
            .entry(domain.to_string())
            .or_default()
            .insert(key.to_string(), value);
        if let Some(keys) = self.ignore.get_mut(domain) {
            keys.remove(key);
        }
    }

    #[must_use]
    pub fn ignored(&self, domain: &str, key: &str) -> bool {
        self.ignore
            .get(domain)
            .is_some_and(|keys| keys.contains(key))
    }

    pub fn add_ignore(&mut self, domain: &str, key: &str) {
        if let Some(keys) = self.settings.get_mut(domain) {
            keys.remove(key);
        }
        self.ignore
            .entry(domain.to_string())
            .or_default()
            .insert(key.to_string());
    }

    /// Drop a recorded value (a lane move, not an opt-out).
    pub fn remove(&mut self, domain: &str, key: &str) -> bool {
        if let Some(keys) = self.restarts.get_mut(domain) {
            keys.remove(key);
            if keys.is_empty() {
                self.restarts.remove(domain);
            }
        }
        self.settings
            .get_mut(domain)
            .is_some_and(|keys| keys.remove(key).is_some())
    }

    pub fn set_restart(&mut self, domain: &str, key: &str, process: &str) {
        self.restarts
            .entry(domain.to_string())
            .or_default()
            .insert(key.to_string(), process.to_string());
    }

    #[must_use]
    pub fn restart_of(&self, domain: &str, key: &str) -> Option<&str> {
        self.restarts
            .get(domain)
            .and_then(|keys| keys.get(key))
            .map(String::as_str)
    }

    pub fn remove_ignore(&mut self, domain: &str, key: &str) -> bool {
        self.ignore
            .get_mut(domain)
            .is_some_and(|keys| keys.remove(key))
    }

    /// Every governed (domain, key) pair: corpus plus manifest.
    #[must_use]
    pub fn governed_keys(&self) -> BTreeSet<(String, String)> {
        let mut out: BTreeSet<(String, String)> = CORPUS
            .iter()
            .map(|s| (s.domain.to_string(), s.key.to_string()))
            .collect();
        for (domain, keys) in &self.settings {
            for key in keys.keys() {
                out.insert((domain.clone(), key.clone()));
            }
        }
        out
    }
}

/// The preferences plist for a domain. `NSGlobalDomain` is spelled
/// `.GlobalPreferences` on disk.
#[must_use]
pub fn plist_path(prefs_dir: &Path, domain: &str) -> PathBuf {
    if domain == "NSGlobalDomain" {
        prefs_dir.join(".GlobalPreferences.plist")
    } else {
        prefs_dir.join(format!("{domain}.plist"))
    }
}

/// Read the current values for a set of governed keys, straight from
/// the plists. A missing domain or key simply yields no entry; an
/// unsupported value type is skipped (we do not govern what we cannot
/// represent).
#[must_use]
pub fn read_current(
    prefs_dir: &Path,
    wanted: &BTreeSet<(String, String)>,
) -> BTreeMap<(String, String), Value> {
    let mut out = BTreeMap::new();
    let mut by_domain: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (domain, key) in wanted {
        by_domain.entry(domain).or_default().push(key);
    }
    for (domain, keys) in by_domain {
        let Ok(plist::Value::Dictionary(dict)) =
            plist::Value::from_file(plist_path(prefs_dir, domain))
        else {
            continue;
        };
        for key in keys {
            // Governed keys read at full fidelity: a recorded array or
            // dict is a first-class value here (ambient discovery and
            // capture stay scalar elsewhere).
            if let Some(value) = dict.get(key).and_then(Value::from_plist_any) {
                out.insert((domain.to_string(), key.to_string()), value);
            }
        }
    }
    out
}

/// Domain name for a plist file, inverting `plist_path`'s spelling.
fn domain_of(file_name: &str) -> Option<&str> {
    if file_name == ".GlobalPreferences.plist" {
        return Some("NSGlobalDomain");
    }
    file_name.strip_suffix(".plist")
}

/// Read EVERY top-level scalar key in every domain — the capture
/// snapshot. Deliberately bounded to what wukong can govern: top-level
/// scalars (nested state and arrays are app furniture, and `defaults
/// write` cannot address them cleanly anyway).
#[must_use]
pub fn read_all(prefs_dir: &Path) -> BTreeMap<(String, String), Value> {
    let mut out = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(prefs_dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(domain) = domain_of(&name) else {
            continue;
        };
        let Ok(plist::Value::Dictionary(dict)) = plist::Value::from_file(entry.path()) else {
            continue;
        };
        for (key, raw) in &dict {
            if let Some(value) = Value::from_plist(raw) {
                out.insert((domain.to_string(), key.clone()), value);
            }
        }
    }
    out
}

/// Keys that are app furniture, not settings: window geometry, panel
/// state, update timestamps, session identifiers. Capture filters
/// these from the signal (they stay visible behind `--all`). The list
/// is curated and deliberately conservative — a false "noise" verdict
/// hides a real setting, a false "signal" merely adds a line.
const NOISE_KEY_MARKERS: &[&str] = &[
    "NSWindow Frame",
    "NSNav Panel",
    "NSNavLast",
    "NSNavRecent",
    "NSToolbar Configuration",
    "NSSplitView",
    "NSTableView Columns",
    "NSTableView Sort",
    "NSTableView Supports",
    "NSOutlineView Items",
    "NSStatusItem",
    "QuickLook",
    "SULastCheck",
    "LastUsed",
    "LastOpened",
    "LastAttempt",
    "LastRun",
    "LastScan",
    "LastVacuum",
    "Timestamp",
    "SessionID",
    "session-id",
    "-uuid",
    "UUID",
    "TrialArm",
    "CKPerBootTasks",
    "seed-",
];

/// Whole domains that are pure churn.
const NOISE_DOMAINS: &[&str] = &[
    "com.apple.EmojiCache",
    "com.apple.spaces",
    "ContextStoreAgent",
    "com.apple.xpc.activity2",
    "com.apple.knowledge-agent",
    "com.apple.identityservices.idstatuscache",
    "com.apple.CloudKit",
    "com.apple.security.KCN",
];

/// Is this changed key app furniture rather than a setting?
#[must_use]
pub fn is_noise_key(domain: &str, key: &str) -> bool {
    if NOISE_DOMAINS.iter().any(|d| domain.starts_with(d)) {
        return true;
    }
    NOISE_KEY_MARKERS.iter().any(|m| key.contains(m))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complex_values_are_structural_citizens() {
        let dock = plist::Value::Array(vec![plist::Value::Dictionary({
            let mut d = plist::Dictionary::new();
            d.insert("tile-type".into(), plist::Value::String("file-tile".into()));
            d
        })]);
        let v = Value::from_plist_any(&dock).unwrap();
        let Value::Complex { plist } = &v else {
            panic!("array must read as complex");
        };
        assert!(plist.contains("tile-type"));

        // Structural equality survives XML re-spelling.
        let respelled = Value::Complex {
            plist: plist.replace('\n', "\n "),
        };
        assert!(v.matches(&respelled));
        // …and an empty array is a different value.
        let empty = Value::from_plist_any(&plist::Value::Array(vec![])).unwrap();
        assert!(!v.matches(&empty));
        assert_eq!(empty.to_string(), "«array, 0 item(s)»");

        // defaults gets plist text, no type flag.
        assert_eq!(empty.defaults_args().len(), 1);

        // The manifest round-trips it (untagged serde stays
        // unambiguous: {plist} table vs bare scalar).
        let mut m = SettingsManifest::default();
        m.set("com.apple.dock", "persistent-apps", v.clone());
        m.set("com.apple.dock", "autohide", Value::Bool(true));
        let toml_text = toml::to_string_pretty(&m).unwrap();
        let back: SettingsManifest = toml::from_str(&toml_text).unwrap();
        assert_eq!(back.desired("com.apple.dock", "persistent-apps"), Some(&v));
        assert_eq!(
            back.desired("com.apple.dock", "autohide"),
            Some(&Value::Bool(true))
        );
    }

    #[test]
    fn ambient_paths_stay_scalar() {
        let arr = plist::Value::Array(vec![]);
        assert!(
            Value::from_plist(&arr).is_none(),
            "capture must not see arrays"
        );
    }

    #[test]
    fn read_all_sees_every_domain_and_only_scalars() {
        let tmp = tempfile::TempDir::new().unwrap();
        let prefs = tmp.path();
        let mut dock = plist::Dictionary::new();
        dock.insert("autohide".into(), plist::Value::Boolean(true));
        dock.insert("furniture".into(), plist::Value::Array(vec![]));
        plist::Value::Dictionary(dock)
            .to_file_binary(plist_path(prefs, "com.apple.dock"))
            .unwrap();
        let mut global = plist::Dictionary::new();
        global.insert("KeyRepeat".into(), plist::Value::Integer(2.into()));
        plist::Value::Dictionary(global)
            .to_file_binary(plist_path(prefs, "NSGlobalDomain"))
            .unwrap();
        std::fs::write(prefs.join("not-a-plist.txt"), "ignored").unwrap();

        let all = read_all(prefs);
        assert_eq!(all.len(), 2);
        assert!(all.contains_key(&("com.apple.dock".to_string(), "autohide".to_string())));
        // The global domain's on-disk spelling maps back to its name.
        assert!(all.contains_key(&("NSGlobalDomain".to_string(), "KeyRepeat".to_string())));
    }

    #[test]
    fn noise_filter_is_conservative() {
        assert!(is_noise_key("com.apple.finder", "NSWindow Frame main"));
        assert!(is_noise_key("com.apple.dock", "SULastCheckTime"));
        assert!(is_noise_key("com.apple.spaces", "anything"));
        // Real settings survive.
        assert!(!is_noise_key("com.apple.dock", "autohide"));
        assert!(!is_noise_key("NSGlobalDomain", "KeyRepeat"));
        assert!(!is_noise_key("NSGlobalDomain", "InitialKeyRepeat"));
        // Column-state chaff is noise; the row-size SETTING is not.
        assert!(is_noise_key(
            "com.apple.finder",
            "NSTableView Columns v2 something"
        ));
        assert!(!is_noise_key(
            "NSGlobalDomain",
            "NSTableViewDefaultSizeMode"
        ));
        assert!(is_noise_key(
            "com.apple.mail",
            "NSToolbar Configuration browser"
        ));
        assert!(!is_noise_key(
            "NSGlobalDomain",
            "NSToolbarTitleViewRolloverDelay"
        ));
        assert!(!is_noise_key(
            "NSGlobalDomain",
            "NSNavPanelExpandedStateForSaveMode"
        ));
        // Every corpus key must be signal, or capture would hide it.
        for s in CORPUS {
            assert!(
                !is_noise_key(s.domain, s.key),
                "{}/{} misfiled",
                s.domain,
                s.key
            );
        }
    }

    #[test]
    fn corpus_is_coherent() {
        let mut seen = BTreeSet::new();
        for s in CORPUS {
            assert!(
                seen.insert((s.domain, s.key)),
                "duplicate corpus entry {}/{}",
                s.domain,
                s.key
            );
            assert!(!s.label.is_empty() && !s.group.is_empty());
        }
        assert!(CORPUS.len() >= 80, "corpus unexpectedly small");
    }

    #[test]
    fn values_match_across_representations() {
        assert!(Value::Bool(true).matches(&Value::Int(1)));
        assert!(Value::Int(0).matches(&Value::Bool(false)));
        assert!(!Value::Bool(true).matches(&Value::Int(2)));
        assert!(Value::Float(0.15).matches(&Value::Float(0.15 + 1e-12)));
        assert!(Value::Int(48).matches(&Value::Float(48.0)));
        assert!(!Value::Str("a".into()).matches(&Value::Int(1)));
    }

    #[test]
    fn manifest_round_trips_and_reads_plists() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mut m = SettingsManifest::default();
        m.set("com.apple.dock", "autohide", Value::Bool(true));
        m.set("NSGlobalDomain", "KeyRepeat", Value::Int(2));
        m.add_ignore("com.apple.finder", "CreateDesktop");
        m.save(&tmp.path().join("store")).unwrap();
        let loaded = SettingsManifest::load(&tmp.path().join("store"))
            .unwrap()
            .unwrap();
        assert_eq!(loaded, m);
        assert!(loaded.ignored("com.apple.finder", "CreateDesktop"));

        // plist round trip, binary format, global-domain spelling.
        let prefs = tmp.path().join("prefs");
        std::fs::create_dir_all(&prefs).unwrap();
        let mut dict = plist::Dictionary::new();
        dict.insert("autohide".into(), plist::Value::Boolean(true));
        dict.insert("tilesize".into(), plist::Value::Integer(48.into()));
        dict.insert("ignored-array".into(), plist::Value::Array(vec![]));
        plist::Value::Dictionary(dict)
            .to_file_binary(plist_path(&prefs, "com.apple.dock"))
            .unwrap();
        let wanted: BTreeSet<_> = [
            ("com.apple.dock".to_string(), "autohide".to_string()),
            ("com.apple.dock".to_string(), "tilesize".to_string()),
            ("com.apple.dock".to_string(), "ignored-array".to_string()),
            ("com.apple.dock".to_string(), "absent".to_string()),
        ]
        .into();
        let current = read_current(&prefs, &wanted);
        // Governed keys read at FULL fidelity: the array arrives as a
        // complex value (absent keys stay absent).
        assert_eq!(current.len(), 3);
        assert!(matches!(
            current[&("com.apple.dock".to_string(), "ignored-array".to_string())],
            Value::Complex { .. }
        ));
        assert!(
            current[&("com.apple.dock".to_string(), "autohide".to_string())]
                .matches(&Value::Bool(true))
        );
    }
}
