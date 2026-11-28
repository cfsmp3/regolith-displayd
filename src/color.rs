//! Display colour management for the Wayland session.
//!
//! In a GNOME Wayland session the compositor (Mutter's colour manager)
//! registers every monitor with colord, the system colour daemon, imports
//! the ICC profiles the user has collected, and puts each monitor's default
//! profile into effect. GNOME Settings' Color panel is a colord client that
//! relies on all of this. Under sway nobody does it, so regolith-displayd,
//! which already stands in for Mutter's display configuration, does it too:
//!
//! - every output sway lists gets a `temp` colord device with the id Mutter
//!   gives the monitor, `xrandr-<make>-<model>-<serial>`, so the profile
//!   colord keeps assigned to a monitor follows it to any connector and to
//!   any session (see [`device_id`]); two outputs showing the same monitor
//!   strings are told apart by the connector, as Mutter does;
//! - the ICC files in `$XDG_DATA_HOME/icc` become `temp` colord profiles
//!   with the ids gnome-settings-daemon gives them, `icc-<md5>`, and follow
//!   their files as they are rewritten and removed;
//! - each output is rendered with the first of its device's `Profiles`, its
//!   default, through sway's per-output `color_profile`, and sRGB when there
//!   is none, while a calibration tool inhibits the device, or while the
//!   device is disabled. sway loses that profile when its configuration is
//!   reloaded, so it is applied again then, and again when an output is
//!   turned back on, in case its configuration was replaced meanwhile. The
//!   file of the default is watched too: rewritten, it is loaded again, and
//!   removed, the output goes back to sRGB.
//!
//! The service runs on its own thread, like Night Light, so colord being
//! absent, late or restarting never disturbs the `DisplayConfig` service.
//! While colord is away the outputs keep the profiles they have.

use crate::{home_dir, PrimaryMonitor};
use log::{debug, info, warn};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use swayipc_async::{Connection as SwayConnection, Output};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use zbus::export::futures_util::StreamExt;
use zbus::fdo::DBusProxy;
use zbus::names::{BusName, OwnedUniqueName, WellKnownName};
use zbus::{dbus_proxy, CacheProperties};
use zvariant::{ObjectPath, OwnedObjectPath};

const COLORD: &str = "org.freedesktop.ColorManager";

/// How often the outputs, the ICC directory and colord are looked at when
/// no event comes earlier.
const SYNC_INTERVAL: Duration = Duration::from_millis(500);

/// How long to wait before trying again to follow sway's events, or to
/// register an output that colord refused.
const RETRY_DELAY: Duration = Duration::from_secs(5);

#[dbus_proxy(
    interface = "org.freedesktop.ColorManager",
    default_service = "org.freedesktop.ColorManager",
    default_path = "/org/freedesktop/ColorManager"
)]
trait ColorManager {
    fn create_device(
        &self,
        device_id: &str,
        scope: &str,
        properties: HashMap<&str, &str>,
    ) -> zbus::Result<OwnedObjectPath>;

    fn delete_device(&self, object_path: &ObjectPath<'_>) -> zbus::Result<()>;

    fn create_profile(
        &self,
        profile_id: &str,
        scope: &str,
        properties: HashMap<&str, &str>,
    ) -> zbus::Result<OwnedObjectPath>;

    fn delete_profile(&self, object_path: &ObjectPath<'_>) -> zbus::Result<()>;

    fn find_profile_by_id(&self, profile_id: &str) -> zbus::Result<OwnedObjectPath>;

    #[dbus_proxy(signal)]
    fn device_changed(&self, object_path: ObjectPath<'_>) -> zbus::Result<()>;
}

#[dbus_proxy(
    interface = "org.freedesktop.ColorManager.Device",
    default_service = "org.freedesktop.ColorManager"
)]
trait Device {
    fn set_property(&self, property_name: &str, property_value: &str) -> zbus::Result<()>;

    #[dbus_proxy(property)]
    fn profiles(&self) -> zbus::Result<Vec<OwnedObjectPath>>;

    /// Settings' per-display switch.
    #[dbus_proxy(property)]
    fn enabled(&self) -> zbus::Result<bool>;

    /// The clients that inhibit the device while they measure the display.
    #[dbus_proxy(property)]
    fn profiling_inhibitors(&self) -> zbus::Result<Vec<String>>;
}

#[dbus_proxy(
    interface = "org.freedesktop.ColorManager.Profile",
    default_service = "org.freedesktop.ColorManager"
)]
trait Profile {
    #[dbus_proxy(property)]
    fn filename(&self) -> zbus::Result<String>;
}

/// What an output is rendered with.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Rendering {
    /// sway's default, sRGB.
    Srgb,
    /// The ICC profile in this file.
    Icc(String),
}

impl fmt::Display for Rendering {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Rendering::Srgb => write!(f, "sRGB"),
            Rendering::Icc(path) => write!(f, "the ICC profile {path}"),
        }
    }
}

/// A file as it was looked at: a different stamp is another version of it,
/// rewritten in place or replaced.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FileStamp {
    /// There is no such file.
    Missing,
    /// The file cannot be looked at, for another reason.
    Unknown,
    /// The device and inode of the file, its size, and the times of its last
    /// modification and status change.
    Version(u64, u64, u64, (i64, i64), (i64, i64)),
}

impl FileStamp {
    /// The file at `path` now, following symlinks.
    fn of(path: &Path) -> FileStamp {
        use std::os::unix::fs::MetadataExt;
        match fs::metadata(path) {
            Ok(m) => FileStamp::Version(
                m.dev(),
                m.ino(),
                m.len(),
                (m.mtime(), m.mtime_nsec()),
                (m.ctime(), m.ctime_nsec()),
            ),
            Err(e) if is_missing(&e) => FileStamp::Missing,
            Err(_) => FileStamp::Unknown,
        }
    }
}

/// Whether `error` says that there is no file at the path.
fn is_missing(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound
        || error.raw_os_error() == Some(rustix::io::Errno::NOTDIR.raw_os_error())
}

/// A device's default profile, as colord has it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DefaultProfile {
    /// The first of the device's `Profiles`.
    profile: Option<OwnedObjectPath>,
    /// Its `Filename`.
    file: Option<String>,
    /// Why no profile is to be applied to the output for now, whatever the
    /// default: a calibration tool inhibits the device, or the device is
    /// disabled.
    suspended: Option<&'static str>,
}

/// What wakes the service up before [`SYNC_INTERVAL`] is over.
enum Wake {
    /// sway's outputs changed, or this service's own commands were applied.
    Outputs,
    /// sway reloaded its configuration, which drops the colour profiles.
    Reload,
    /// A colord device changed: its default profile may have.
    DeviceChanged(OwnedObjectPath),
}

/// One of sway's outputs, and the colord device of its monitor.
struct Display {
    /// The colord device id of the monitor strings (see [`device_id`]).
    base_id: String,
    /// The colord device id of the output: `base_id`, followed by a dash and
    /// the connector when another output had `base_id` first.
    device_id: String,
    /// The monitor's make, model and serial, those sway reports.
    vendor: Option<String>,
    model: Option<String>,
    serial: Option<String>,
    /// Whether the output is on.
    active: bool,
    /// The colord device, while colord has it.
    device: Option<OwnedObjectPath>,
    /// When colord last refused to create the device.
    refused: Option<Instant>,
    /// The device's `OutputPriority`.
    priority: &'static str,
    /// Whether the device's default profile has to be read again.
    stale: bool,
    /// The device's default profile, once it has been read. Another profile
    /// object is a new assignment, even with the same file.
    default: Option<DefaultProfile>,
    /// Whether the file of the default profile was found gone. The output
    /// stays on sRGB until another profile is the default.
    missing: bool,
    /// The file of the default profile as it was when it was last given to
    /// sway: another version of it is loaded again.
    stamp: Option<FileStamp>,
    /// What sway renders the output with, as far as this service knows.
    rendered: Option<Rendering>,
    /// What sway refused to render the output with: a profile it could not
    /// load, or sRGB when it has no per-output colour profiles (before sway
    /// 1.11). Not tried again until colord signals a change of the device,
    /// which may be a new assignment of a repaired file, until sway may have
    /// dropped the profile, or until the file changes.
    rejected: Option<Rendering>,
}

impl Display {
    fn new(output: &Output) -> Display {
        let part = |part: &str| identity_part(part).map(str::to_string);
        Display {
            base_id: device_id(output),
            device_id: device_id(output),
            vendor: part(&output.make),
            model: part(&output.model),
            serial: part(&output.serial),
            active: output.active,
            device: None,
            refused: None,
            priority: "secondary",
            stale: false,
            default: None,
            missing: false,
            stamp: None,
            rendered: None,
            rejected: None,
        }
    }

    /// What the output is to be rendered with, once the device's default
    /// profile has been read: that profile, unless the device is inhibited
    /// or disabled, or the profile's file is gone; sRGB otherwise.
    fn wanted(&self) -> Option<Rendering> {
        let default = self.default.as_ref()?;
        Some(match &default.file {
            Some(file) if default.suspended.is_none() && !self.missing => {
                Rendering::Icc(file.clone())
            }
            _ => Rendering::Srgb,
        })
    }

    /// sway may have dropped the output's profile: apply it again.
    fn forget_rendering(&mut self) {
        self.rendered = None;
        self.rejected = None;
    }
}

/// A make, model or serial string as sway reports it, unless it reports none.
fn identity_part(part: &str) -> Option<&str> {
    match part {
        "" | "Unknown" => None,
        part => Some(part),
    }
}

/// The colord device id of the monitor on `output`, the one Mutter gives it:
/// "xrandr" followed by the make, model and serial sway reports, those it
/// knows, each after a dash; "xrandr-" and the connector name when it knows
/// none. The same monitor gets the same id on any connector, so the profile
/// colord keeps assigned to it follows it.
fn device_id(output: &Output) -> String {
    let parts: Vec<&str> = [&output.make, &output.model, &output.serial]
        .into_iter()
        .filter_map(|part| identity_part(part))
        .collect();
    if parts.is_empty() {
        format!("xrandr-{}", output.name)
    } else {
        format!("xrandr-{}", parts.join("-"))
    }
}

/// The user's ICC directory: `$XDG_DATA_HOME/icc`, `~/.local/share/icc` by
/// default.
fn icc_directory() -> Option<PathBuf> {
    let data_home = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| home_dir().map(|home| home.join(".local").join("share")))?;
    Some(data_home.join("icc"))
}

/// Whether a file of the ICC directory with this name is imported: not
/// hidden, not a backup, and not a profile generated from a monitor's EDID,
/// as gnome-settings-daemon decides.
fn importable_name(name: &str) -> bool {
    !name.starts_with('.') && !name.ends_with('~') && !name.starts_with("edid-")
}

/// Whether `data` is an ICC profile: its header has the signature "acsp" at
/// offset 36.
fn is_icc(data: &[u8]) -> bool {
    data.get(36..40) == Some(b"acsp".as_slice())
}

/// Whether the ICC profile `data` is shorter than its header says: its file
/// is still being written.
fn is_incomplete(data: &[u8]) -> bool {
    let size = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
    usize::try_from(size).is_ok_and(|size| size > data.len())
}

/// The MD5 digest of `data` (RFC 1321) in lower-case hexadecimal, which
/// colord and gnome-settings-daemon identify ICC files by.
fn md5_hex(data: &[u8]) -> String {
    const SHIFTS: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, //
        5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, 5, 9, 14, 20, //
        4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, //
        6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    const SINES: [u32; 64] = [
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, //
        0xf57c0faf, 0x4787c62a, 0xa8304613, 0xfd469501, //
        0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, //
        0x6b901122, 0xfd987193, 0xa679438e, 0x49b40821, //
        0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, //
        0xd62f105d, 0x02441453, 0xd8a1e681, 0xe7d3fbc8, //
        0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed, //
        0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a, //
        0xfffa3942, 0x8771f681, 0x6d9d6122, 0xfde5380c, //
        0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70, //
        0x289b7ec6, 0xeaa127fa, 0xd4ef3085, 0x04881d05, //
        0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665, //
        0xf4292244, 0x432aff97, 0xab9423a7, 0xfc93a039, //
        0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1, //
        0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, //
        0xf7537e82, 0xbd3af235, 0x2ad7d2bb, 0xeb86d391,
    ];
    let mut state: [u32; 4] = [0x67452301, 0xefcdab89, 0x98badcfe, 0x10325476];
    let mut message = data.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&((data.len() as u64).wrapping_mul(8)).to_le_bytes());
    for block in message.chunks_exact(64) {
        let words: Vec<u32> = block
            .chunks_exact(4)
            .map(|word| u32::from_le_bytes([word[0], word[1], word[2], word[3]]))
            .collect();
        let [mut a, mut b, mut c, mut d] = state;
        for i in 0..64 {
            let (f, g) = match i / 16 {
                0 => ((b & c) | (!b & d), i),
                1 => ((d & b) | (!d & c), (5 * i + 1) % 16),
                2 => (b ^ c ^ d, (3 * i + 5) % 16),
                _ => (c ^ (b | !d), (7 * i) % 16),
            };
            let rotated = a
                .wrapping_add(f)
                .wrapping_add(SINES[i])
                .wrapping_add(words[g])
                .rotate_left(SHIFTS[i]);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(rotated);
        }
        for (value, added) in state.iter_mut().zip([a, b, c, d]) {
            *value = value.wrapping_add(added);
        }
    }
    state
        .iter()
        .flat_map(|word| word.to_le_bytes())
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// `path` as the file argument of sway's `color_profile icc`, whatever
/// characters it holds (but a newline).
///
/// sway splits a command at the `;`, `,` and blanks outside quotes, takes
/// the quotes off its arguments, keeping backslashes, and then replaces the
/// `$variables` in them, unless the `$` follows exactly one backslash, and
/// turns `$$` into `$`. It expands the file argument like a shell word
/// (wordexp) at last. So every ASCII character but letters, digits and
/// `/._-+` is preceded by a backslash, which both sway and wordexp respect,
/// a `$` that follows a backslash of the path is doubled, and the whole is
/// quoted.
fn sway_file_argument(path: &str) -> String {
    let mut argument = String::from("\"");
    let mut previous = None;
    for c in path.chars() {
        if c.is_ascii() && !c.is_ascii_alphanumeric() && !"/._-+".contains(c) {
            argument.push('\\');
        }
        argument.push(c);
        if c == '$' && previous == Some('\\') {
            argument.push('$');
        }
        previous = Some(c);
    }
    argument.push('"');
    argument
}

/// The sway command that renders `output` with `rendering`.
fn sway_command(output: &str, rendering: &Rendering) -> String {
    match rendering {
        Rendering::Srgb => format!("output \"{output}\" color_profile srgb"),
        Rendering::Icc(path) => format!(
            "output \"{output}\" color_profile icc {}",
            sway_file_argument(path)
        ),
    }
}

/// Run `command` in sway, connecting first if needed.
async fn run_sway(sway: &mut Option<SwayConnection>, command: &str) -> Result<(), String> {
    if sway.is_none() {
        *sway = Some(SwayConnection::new().await.map_err(|e| e.to_string())?);
    }
    let connection = sway.as_mut().expect("connected just above");
    match connection.run_command(command).await {
        Ok(outcomes) => outcomes
            .into_iter()
            .try_for_each(|outcome| outcome.map_err(|e| e.to_string())),
        Err(e) => {
            *sway = None;
            Err(e.to_string())
        }
    }
}

/// sway's IPC message header, and the message types this service uses.
const IPC_MAGIC: &[u8; 6] = b"i3-ipc";
const IPC_SUBSCRIBE: u32 = 2;
const IPC_WORKSPACE_EVENT: u32 = 0x8000_0000;
const IPC_OUTPUT_EVENT: u32 = 0x8000_0001;

async fn ipc_send(stream: &mut UnixStream, kind: u32, payload: &[u8]) -> io::Result<()> {
    let length = u32::try_from(payload.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "message too long"))?;
    let mut message = Vec::with_capacity(14 + payload.len());
    message.extend_from_slice(IPC_MAGIC);
    message.extend_from_slice(&length.to_ne_bytes());
    message.extend_from_slice(&kind.to_ne_bytes());
    message.extend_from_slice(payload);
    stream.write_all(&message).await
}

async fn ipc_receive(stream: &mut UnixStream) -> io::Result<(u32, Vec<u8>)> {
    let mut header = [0u8; 14];
    stream.read_exact(&mut header).await?;
    if &header[..6] != IPC_MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a sway IPC message",
        ));
    }
    let length = u32::from_ne_bytes([header[6], header[7], header[8], header[9]]);
    let kind = u32::from_ne_bytes([header[10], header[11], header[12], header[13]]);
    let mut payload = vec![0; length as usize];
    stream.read_exact(&mut payload).await?;
    Ok((kind, payload))
}

#[derive(Deserialize)]
struct IpcSuccess {
    success: bool,
}

#[derive(Deserialize)]
struct IpcChange {
    change: String,
}

/// Wake the service up on sway's output events and configuration reloads.
///
/// An output turned off and on again between two looks would go unnoticed,
/// so the outputs are looked at on each output event. That includes the
/// events this service's own commands cause, which change nothing it looks
/// at. swayipc-async knows no output events: this speaks sway's IPC itself.
async fn follow_sway_events(wakes: mpsc::UnboundedSender<Wake>) {
    loop {
        if let Err(e) = sway_events(&wakes).await {
            debug!("Not following sway's events: {e}");
        }
        if wakes.is_closed() {
            return;
        }
        tokio::time::sleep(RETRY_DELAY).await;
    }
}

async fn sway_events(wakes: &mpsc::UnboundedSender<Wake>) -> io::Result<()> {
    let socket = std::env::var_os("SWAYSOCK")
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "SWAYSOCK is not set"))?;
    let mut stream = UnixStream::connect(socket).await?;
    ipc_send(&mut stream, IPC_SUBSCRIBE, br#"["output","workspace"]"#).await?;
    let (_, reply) = ipc_receive(&mut stream).await?;
    if !serde_json::from_slice::<IpcSuccess>(&reply).is_ok_and(|reply| reply.success) {
        return Err(io::Error::other("sway refused the subscription"));
    }
    // sway may have reloaded since the outputs were last rendered.
    let _ = wakes.send(Wake::Reload);
    loop {
        let (kind, payload) = ipc_receive(&mut stream).await?;
        let wake = match kind {
            IPC_OUTPUT_EVENT => Wake::Outputs,
            IPC_WORKSPACE_EVENT => match serde_json::from_slice::<IpcChange>(&payload) {
                Ok(event) if event.change == "reload" => Wake::Reload,
                _ => continue,
            },
            _ => continue,
        };
        if wakes.send(wake).is_err() {
            return Ok(());
        }
    }
}

/// Whether colord refused to create an object because one with that id
/// exists already.
fn already_exists(error: &zbus::Error) -> bool {
    matches!(error, zbus::Error::MethodError(name, _, _)
        if name.as_str() == "org.freedesktop.ColorManager.AlreadyExists")
}

/// The system bus, as far as this service uses it.
struct SystemBus {
    connection: zbus::Connection,
    dbus: DBusProxy<'static>,
}

impl SystemBus {
    async fn connect() -> zbus::Result<SystemBus> {
        let connection = zbus::Connection::system().await?;
        let dbus = DBusProxy::builder(&connection)
            .cache_properties(CacheProperties::No)
            .build()
            .await?;
        // Have colord started if it is activatable and not running yet, as
        // GNOME's colord clients do.
        if let Err(e) = dbus
            .start_service_by_name(WellKnownName::from_static_str(COLORD)?, 0)
            .await
        {
            debug!("Cannot activate {COLORD}: {e}");
        }
        Ok(SystemBus { connection, dbus })
    }

    /// The unique name colord has on the bus, if it is on it.
    async fn colord_owner(&self) -> zbus::Result<Option<OwnedUniqueName>> {
        match self
            .dbus
            .get_name_owner(BusName::from_static_str(COLORD)?)
            .await
        {
            Ok(owner) => Ok(Some(owner)),
            Err(zbus::fdo::Error::NameHasNoOwner(_)) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

/// colord, while it is on the system bus. The devices and profiles this
/// service creates go away with it.
struct Colord {
    /// colord's unique name: another one is a restarted colord.
    owner: OwnedUniqueName,
    manager: ColorManagerProxy<'static>,
    /// Forwards the devices' changes as [`Wake::DeviceChanged`].
    changes: JoinHandle<()>,
    /// The files of the ICC directory looked at.
    files: HashMap<PathBuf, StoreFile>,
}

/// A file of the ICC directory, as it was last looked at.
struct StoreFile {
    /// The version looked at.
    version: FileStamp,
    /// The id and object of the profile that version is imported as, if it
    /// is one colord accepted.
    profile: Option<(String, OwnedObjectPath)>,
}

impl Colord {
    async fn follow(
        bus: &zbus::Connection,
        owner: OwnedUniqueName,
        wakes: mpsc::UnboundedSender<Wake>,
    ) -> zbus::Result<Colord> {
        let manager = ColorManagerProxy::builder(bus)
            .cache_properties(CacheProperties::No)
            .build()
            .await?;
        // Subscribe before creating anything, so that no change is missed.
        let mut device_changes = manager.receive_device_changed().await?;
        let changes = tokio::spawn(async move {
            while let Some(signal) = device_changes.next().await {
                let Ok(args) = signal.args() else { continue };
                let device = OwnedObjectPath::from(args.object_path().to_owned());
                if wakes.send(Wake::DeviceChanged(device)).is_err() {
                    return;
                }
            }
        });
        Ok(Colord {
            owner,
            manager,
            changes,
            files: HashMap::new(),
        })
    }

    fn bus(&self) -> &zbus::Connection {
        self.manager.connection()
    }

    /// Bring the profiles of the ICC files of `directory` up to date: each
    /// file not imported yet, or changed since, becomes the `temp` profile
    /// `icc-<md5>` with its path, and the profile of a file that changed or
    /// went away is removed, as gnome-settings-daemon does.
    async fn import_icc_files(&mut self, directory: &Path) {
        let mut present = HashSet::new();
        match fs::read_dir(directory) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    if let Some(path) = self.import_icc_file(directory, &entry).await {
                        present.insert(path);
                    }
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => create_icc_directory(directory),
            Err(e) => {
                debug!("Cannot read {}: {e}", directory.display());
                return;
            }
        }
        let gone: Vec<PathBuf> = self
            .files
            .keys()
            .filter(|path| !present.contains(*path))
            .cloned()
            .collect();
        for path in gone {
            let Some(file) = self.files.remove(&path) else {
                continue;
            };
            if let Some((id, object)) = file.profile {
                info!("The ICC profile {} is gone", path.display());
                self.retire_profile(&id, &object).await;
            }
        }
    }

    /// Import the file of the ICC directory `entry`, if it is not imported
    /// yet or changed since. Returns its path if it is a file of the
    /// directory to import, whether it is an ICC profile or not.
    async fn import_icc_file(&mut self, directory: &Path, entry: &fs::DirEntry) -> Option<PathBuf> {
        let name = entry.file_name();
        // A D-Bus string is UTF-8: other paths cannot be a Filename.
        let name = name.to_str()?;
        if !importable_name(name) {
            return None;
        }
        let path = directory.join(name);
        // Following symlinks: a symlink to a file is imported, under its own
        // path. Only regular files are opened.
        let metadata = fs::metadata(&path).ok()?;
        if !metadata.is_file() {
            return None;
        }
        let version = FileStamp::of(&path);
        if self.files.get(&path).map(|file| &file.version) == Some(&version) {
            return Some(path);
        }
        let data = match fs::read(&path) {
            Ok(data) => data,
            Err(e) => {
                debug!("Cannot read {}: {e}", path.display());
                return Some(path);
            }
        };
        if is_icc(&data) && is_incomplete(&data) {
            // Still being written: look again next time.
            return Some(path);
        }
        let profile = if is_icc(&data) {
            self.import_icc_data(&path, &data).await
        } else {
            debug!("{} is not an ICC profile", path.display());
            None
        };
        let replaced = self
            .files
            .insert(
                path.clone(),
                StoreFile {
                    version,
                    profile: profile.clone(),
                },
            )
            .and_then(|file| file.profile);
        if let Some((id, object)) = replaced {
            if profile.as_ref().map(|(new, _)| new) != Some(&id) {
                info!(
                    "The ICC profile {} changed: it is no longer {id}",
                    path.display()
                );
                self.retire_profile(&id, &object).await;
            }
        }
        Some(path)
    }

    /// Import `data`, the contents of the ICC file `path`, as the `temp`
    /// profile `icc-<md5>`. Returns the profile's id and object, unless
    /// colord refuses it.
    async fn import_icc_data(&self, path: &Path, data: &[u8]) -> Option<(String, OwnedObjectPath)> {
        let filename = path.to_str()?;
        let checksum = md5_hex(data);
        let id = format!("icc-{checksum}");
        let properties =
            HashMap::from([("Filename", filename), ("FILE_checksum", checksum.as_str())]);
        match self.manager.create_profile(&id, "temp", properties).await {
            Ok(object) => {
                info!("Imported the ICC profile {filename} as {id}");
                Some((id, object))
            }
            Err(e) if already_exists(&e) => {
                debug!("The ICC profile {filename} is already there as {id}");
                match self.manager.find_profile_by_id(&id).await {
                    Ok(object) => Some((id, object)),
                    Err(e) => {
                        debug!("Cannot find the ICC profile {id}: {e}");
                        None
                    }
                }
            }
            Err(e) => {
                warn!("Cannot import the ICC profile {filename}: {e}");
                None
            }
        }
    }

    /// Remove the profile `id` of an ICC file that changed or went away,
    /// unless another file of the directory has the same contents.
    async fn retire_profile(&self, id: &str, object: &OwnedObjectPath) {
        let shared = self
            .files
            .values()
            .any(|file| file.profile.as_ref().is_some_and(|(other, _)| other == id));
        if shared {
            return;
        }
        match self.manager.delete_profile(object).await {
            Ok(()) => info!("Removed the ICC profile {id}"),
            Err(e) => debug!("Cannot remove the ICC profile {id}: {e}"),
        }
    }

    /// Create the colour device of the output `name`.
    async fn create_device(
        &self,
        name: &str,
        display: &Display,
        priority: &str,
    ) -> zbus::Result<OwnedObjectPath> {
        let mut properties = HashMap::from([
            ("Kind", "display"),
            ("Mode", "physical"),
            ("Colorspace", "rgb"),
            ("XRANDR_name", name),
            ("OutputPriority", priority),
        ]);
        for (key, value) in [
            ("Vendor", &display.vendor),
            ("Model", &display.model),
            ("Serial", &display.serial),
        ] {
            if let Some(value) = value {
                properties.insert(key, value);
            }
        }
        self.manager
            .create_device(&display.device_id, "temp", properties)
            .await
    }

    async fn device(&self, path: &OwnedObjectPath) -> zbus::Result<DeviceProxy<'static>> {
        DeviceProxy::builder(self.bus())
            .path(path.clone())?
            .cache_properties(CacheProperties::No)
            .build()
            .await
    }

    /// The device's default profile, the first of its `Profiles`, and
    /// whether it may be applied now.
    async fn default_profile(&self, path: &OwnedObjectPath) -> zbus::Result<DefaultProfile> {
        let device = self.device(path).await?;
        // As Mutter does: nothing is applied while a calibration tool
        // measures the display, or while Settings' switch disables it.
        let suspended = if !device.profiling_inhibitors().await?.is_empty() {
            Some("a calibration tool inhibits its colour device")
        } else if !device.enabled().await? {
            Some("its colour device is disabled")
        } else {
            None
        };
        let Some(profile) = device.profiles().await?.into_iter().next() else {
            return Ok(DefaultProfile {
                profile: None,
                file: None,
                suspended,
            });
        };
        let proxy = ProfileProxy::builder(self.bus())
            .path(profile.clone())?
            .cache_properties(CacheProperties::No)
            .build()
            .await?;
        let file = proxy.filename().await?;
        Ok(DefaultProfile {
            profile: Some(profile),
            file: Some(file),
            suspended,
        })
    }
}

impl Drop for Colord {
    fn drop(&mut self) {
        self.changes.abort();
    }
}

fn create_icc_directory(directory: &Path) {
    if let Err(e) = fs::create_dir_all(directory) {
        warn!(
            "Cannot create the ICC directory {}: {e}",
            directory.display()
        );
    }
}

/// What the primary monitor was found for: the number of display changes
/// then, and each output's name, device id, whether it is on, and the size
/// of its mode.
type PrimaryFoundFor = (u64, Vec<(String, String, bool, Option<(i32, i32)>)>);

/// The colour management service: what it knows about the outputs, sway
/// and colord.
struct Service {
    primary_monitor: PrimaryMonitor,
    /// The connector of the primary monitor, and what it was found for.
    primary: Option<String>,
    primary_found_for: Option<PrimaryFoundFor>,
    displays: BTreeMap<String, Display>,
    sway: Option<SwayConnection>,
    icc_directory: Option<PathBuf>,
    system_bus: Option<SystemBus>,
    colord: Option<Colord>,
    /// Whether colord's absence has been logged since it was last there.
    absence_logged: bool,
    wakes: mpsc::UnboundedSender<Wake>,
}

impl Service {
    fn new(primary_monitor: PrimaryMonitor, wakes: mpsc::UnboundedSender<Wake>) -> Service {
        let icc_directory = icc_directory();
        match &icc_directory {
            Some(directory) => create_icc_directory(directory),
            None => warn!("No ICC directory: neither XDG_DATA_HOME nor a home directory is set"),
        }
        Service {
            primary_monitor,
            primary: None,
            primary_found_for: None,
            displays: BTreeMap::new(),
            sway: None,
            icc_directory,
            system_bus: None,
            colord: None,
            absence_logged: false,
            wakes,
        }
    }

    fn wake(&mut self, wake: Wake) {
        match wake {
            Wake::Outputs => {}
            Wake::Reload => self
                .displays
                .values_mut()
                .for_each(Display::forget_rendering),
            Wake::DeviceChanged(path) => {
                for display in self.displays.values_mut() {
                    if display.device.as_ref() == Some(&path) {
                        display.stale = true;
                        // The file sway refused may have been repaired and
                        // assigned again: try it again.
                        display.rejected = None;
                    }
                }
            }
        }
    }

    /// Bring colord's devices and profiles, and what sway renders each
    /// output with, up to date.
    async fn sync(&mut self) {
        let Some(outputs) = self.outputs().await else {
            return;
        };
        let colord = self.follow_colord().await;
        self.follow_outputs(&outputs).await;
        self.find_primary(&outputs).await;
        if colord {
            if let (Some(colord), Some(directory)) = (&mut self.colord, &self.icc_directory) {
                colord.import_icc_files(directory).await;
            }
            self.update_devices().await;
        }
        self.render().await;
    }

    async fn outputs(&mut self) -> Option<Vec<Output>> {
        if self.sway.is_none() {
            match SwayConnection::new().await {
                Ok(connection) => self.sway = Some(connection),
                Err(e) => {
                    warn!("Cannot connect to sway: {e}");
                    return None;
                }
            }
        }
        match self.sway.as_mut()?.get_outputs().await {
            Ok(outputs) => Some(outputs),
            Err(e) => {
                warn!("Cannot read sway's outputs: {e}");
                self.sway = None;
                None
            }
        }
    }

    /// Whether colord is on the system bus. Follows it as it comes, goes and
    /// comes back: each time it comes, its devices and profiles are created
    /// anew.
    async fn follow_colord(&mut self) -> bool {
        if self.system_bus.is_none() {
            match SystemBus::connect().await {
                Ok(bus) => self.system_bus = Some(bus),
                Err(e) => {
                    self.colord_absent(&format!("cannot connect to the system bus: {e}"));
                    return false;
                }
            }
        }
        let bus = self.system_bus.as_ref().expect("connected just above");
        let connection = bus.connection.clone();
        let owner = match bus.colord_owner().await {
            Ok(owner) => owner,
            Err(e) => {
                // Connect again next time.
                self.system_bus = None;
                self.lose_colord();
                self.colord_absent(&format!("the system bus failed: {e}"));
                return false;
            }
        };
        if let Some(colord) = &self.colord {
            if Some(&colord.owner) == owner.as_ref() {
                return true;
            }
            self.lose_colord();
        }
        let Some(owner) = owner else {
            self.colord_absent("it is not running");
            return false;
        };
        match Colord::follow(&connection, owner, self.wakes.clone()).await {
            Ok(colord) => {
                info!("colord is running: registering the displays and importing the ICC profiles");
                self.colord = Some(colord);
                self.absence_logged = false;
                true
            }
            Err(e) => {
                self.colord_absent(&format!("cannot follow it: {e}"));
                false
            }
        }
    }

    fn colord_absent(&mut self, reason: &str) {
        if !self.absence_logged {
            info!("colord is not available ({reason}): display colour profiles are applied once it is");
            self.absence_logged = true;
        }
    }

    /// colord went away, with the devices and profiles: forget them. The
    /// outputs keep what they are rendered with.
    fn lose_colord(&mut self) {
        if self.colord.take().is_some() {
            warn!("colord went away: the displays keep their colour profiles until it is back");
            self.absence_logged = true;
            for display in self.displays.values_mut() {
                display.device = None;
                display.refused = None;
                display.stale = false;
            }
        }
    }

    /// Follow the outputs sway lists: remove the devices of the monitors
    /// unplugged, or replaced by another monitor on their connector, and
    /// note which outputs are on.
    async fn follow_outputs(&mut self, outputs: &[Output]) {
        let gone: Vec<String> = self
            .displays
            .iter()
            .filter(|(name, display)| {
                !outputs
                    .iter()
                    .any(|output| &output.name == *name && device_id(output) == display.base_id)
            })
            .map(|(name, _)| name.clone())
            .collect();
        for name in gone {
            let Some(display) = self.displays.remove(&name) else {
                continue;
            };
            let (Some(device), Some(colord)) = (&display.device, &self.colord) else {
                continue;
            };
            match colord.manager.delete_device(device).await {
                Ok(()) => info!(
                    "{name} is gone: removed its colour device {}",
                    display.device_id
                ),
                Err(e) => warn!(
                    "Cannot remove the colour device {} of {name}: {e}",
                    display.device_id
                ),
            }
        }
        for output in outputs {
            if !self.displays.contains_key(&output.name) {
                let mut display = Display::new(output);
                // Two monitors showing the same strings: as Mutter does, the
                // one registered first keeps the id, and the others get
                // their connector appended, so each has its own device.
                if self
                    .displays
                    .values()
                    .any(|other| other.device_id == display.device_id)
                {
                    display.device_id = format!("{}-{}", display.base_id, output.name);
                }
                self.displays.insert(output.name.clone(), display);
            }
            let display = self
                .displays
                .get_mut(&output.name)
                .expect("inserted just above");
            if !output.active {
                // sway may drop the profile of an output that is off.
                display.forget_rendering();
            }
            display.active = output.active;
        }
    }

    /// Find the primary monitor again when the outputs or the display
    /// configuration changed.
    async fn find_primary(&mut self, outputs: &[Output]) {
        let found_for = (
            self.primary_monitor.changes(),
            outputs
                .iter()
                .map(|output| {
                    (
                        output.name.clone(),
                        device_id(output),
                        output.active,
                        output
                            .current_mode
                            .as_ref()
                            .map(|mode| (mode.width, mode.height)),
                    )
                })
                .collect(),
        );
        if self.primary_found_for.as_ref() == Some(&found_for) {
            return;
        }
        self.primary = self.primary_monitor.connector(outputs).await;
        self.primary_found_for = Some(found_for);
    }

    /// Create the devices colord lacks, keep their `OutputPriority`, and read
    /// the default profile of those created or changed.
    async fn update_devices(&mut self) {
        let Some(colord) = &self.colord else { return };
        for (name, display) in self.displays.iter_mut() {
            let priority = if self.primary.as_deref() == Some(name.as_str()) {
                "primary"
            } else {
                "secondary"
            };
            match &display.device {
                None => {
                    if display.refused.is_some_and(|at| at.elapsed() < RETRY_DELAY) {
                        continue;
                    }
                    match colord.create_device(name, display, priority).await {
                        Ok(path) => {
                            info!(
                                "Registered {name} as the colour device {}",
                                display.device_id
                            );
                            display.device = Some(path);
                            display.refused = None;
                            display.priority = priority;
                            display.stale = true;
                        }
                        Err(e) => {
                            if display.refused.is_none() {
                                warn!(
                                    "Cannot register {name} as the colour device {}: {e}",
                                    display.device_id
                                );
                            }
                            display.refused = Some(Instant::now());
                            continue;
                        }
                    }
                }
                Some(path) if display.priority != priority => {
                    let set = match colord.device(path).await {
                        Ok(device) => device.set_property("OutputPriority", priority).await,
                        Err(e) => Err(e),
                    };
                    match set {
                        Ok(()) => display.priority = priority,
                        Err(e) => warn!("Cannot make the colour device of {name} {priority}: {e}"),
                    }
                }
                Some(_) => {}
            }
            if !display.stale {
                continue;
            }
            let Some(path) = &display.device else {
                continue;
            };
            match colord.default_profile(path).await {
                Ok(default) => {
                    display.stale = false;
                    if display.default.as_ref() == Some(&default) {
                        continue;
                    }
                    let assigned =
                        display.default.as_ref().map(|old| &old.profile) != Some(&default.profile);
                    if assigned {
                        // A new profile, maybe for the same file with other
                        // contents: load it into sway again.
                        display.rendered = None;
                        display.missing = false;
                        display.stamp = None;
                    }
                    match (&default.file, default.suspended) {
                        (Some(file), None) => {
                            info!("The default colour profile of {name} is {file}")
                        }
                        (Some(file), Some(why)) => info!(
                            "The default colour profile of {name} is {file}, \
                             not applied while {why}"
                        ),
                        (None, _) => info!("{name} has no default colour profile"),
                    }
                    display.default = Some(default);
                    display.rejected = None;
                }
                // Changing as it is read: read it again next time.
                Err(e) => debug!("Cannot read the default profile of {name}: {e}"),
            }
        }
    }

    /// Render each output that is on with its device's default profile,
    /// unless sway renders it so already.
    async fn render(&mut self) {
        for (name, display) in self.displays.iter_mut() {
            let Some(mut wanted) = display.wanted() else {
                continue;
            };
            if let Rendering::Icc(path) = &wanted {
                // The file of the profile loaded, or refused, may have been
                // rewritten, replaced or removed since.
                if let Some(stamp) = &display.stamp {
                    let now = FileStamp::of(Path::new(path));
                    if now != *stamp && now != FileStamp::Unknown {
                        if now != FileStamp::Missing {
                            info!("The ICC profile {path} of {name} changed: loading it again");
                        }
                        display.stamp = None;
                        display.rendered = None;
                        display.rejected = None;
                    }
                }
            }
            if !display.active || display.rendered.as_ref() == Some(&wanted) {
                continue;
            }
            if let Rendering::Icc(path) = &wanted {
                if display.rejected.as_ref() == Some(&wanted) {
                    continue;
                }
                let stamp = FileStamp::of(Path::new(path));
                if stamp == FileStamp::Missing {
                    // Gone for good, until another profile is the default.
                    warn!("The ICC profile {path} of {name} is missing: falling back to sRGB");
                    display.missing = true;
                    wanted = Rendering::Srgb;
                    if display.rendered.as_ref() == Some(&wanted) {
                        continue;
                    }
                }
                display.stamp = Some(stamp);
            }
            if display.rejected.as_ref() == Some(&wanted) {
                continue;
            }
            match run_sway(&mut self.sway, &sway_command(name, &wanted)).await {
                Ok(()) => {
                    info!("{name} is rendered with {wanted}");
                    display.rendered = Some(wanted);
                }
                Err(e) => {
                    display.rendered = None;
                    // sway answered and refused the command: wait for a
                    // change before trying it again. A lost connection is
                    // retried at the next look.
                    if self.sway.is_some() {
                        display.rejected = Some(wanted.clone());
                    }
                    let Rendering::Icc(path) = wanted else {
                        warn!("Cannot render {name} with sRGB: {e}");
                        continue;
                    };
                    // Never keep the profile the output had.
                    warn!("Cannot render {name} with the ICC profile {path}: {e}; falling back to sRGB");
                    match run_sway(&mut self.sway, &sway_command(name, &Rendering::Srgb)).await {
                        Ok(()) => display.rendered = Some(Rendering::Srgb),
                        Err(e) => warn!("Cannot render {name} with sRGB: {e}"),
                    }
                }
            }
        }
    }
}

async fn run(primary_monitor: PrimaryMonitor) {
    let (wakes, mut woken) = mpsc::unbounded_channel();
    tokio::spawn(follow_sway_events(wakes.clone()));
    let mut service = Service::new(primary_monitor, wakes);
    loop {
        service.sync().await;
        if let Ok(Some(wake)) = tokio::time::timeout(SYNC_INTERVAL, woken.recv()).await {
            service.wake(wake);
            while let Ok(wake) = woken.try_recv() {
                service.wake(wake);
            }
        }
    }
}

/// Start display colour management on its own thread, for the lifetime of
/// the daemon. It never takes the daemon down.
pub fn spawn(primary_monitor: PrimaryMonitor) {
    let spawned = std::thread::Builder::new()
        .name("color".into())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(e) => {
                    warn!("Display colour management is unavailable: {e}");
                    return;
                }
            };
            runtime.block_on(run(primary_monitor));
        });
    if let Err(e) = spawned {
        warn!("Display colour management is unavailable: cannot start its thread: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn output(name: &str, make: &str, model: &str, serial: &str) -> Output {
        serde_json::from_value(serde_json::json!({
            "id": 1, "name": name, "make": make, "model": model, "serial": serial,
            "active": true, "dpms": true, "primary": false, "scale": 1.0,
            "transform": "normal", "modes": [],
            "rect": {"x": 0, "y": 0, "width": 1920, "height": 1080}
        }))
        .unwrap()
    }

    #[test]
    fn device_id_from_the_monitor_strings() {
        let id = |make, model, serial| device_id(&output("DP-1", make, model, serial));
        assert_eq!(
            id("Acme Corp", "Model 27", "SN1"),
            "xrandr-Acme Corp-Model 27-SN1"
        );
        assert_eq!(id("Acme", "Model", "Unknown"), "xrandr-Acme-Model");
        assert_eq!(id("", "Unknown", "OnlySerial"), "xrandr-OnlySerial");
        assert_eq!(id("Make", "", "Serial"), "xrandr-Make-Serial");
        assert_eq!(id("Unknown", "Unknown", "Unknown"), "xrandr-DP-1");
        assert_eq!(id("", "", ""), "xrandr-DP-1");
    }

    #[test]
    fn missing_monitor_strings_are_unset() {
        let display = Display::new(&output("DP-1", "Acme", "Unknown", ""));
        assert_eq!(display.vendor.as_deref(), Some("Acme"));
        assert_eq!(display.model, None);
        assert_eq!(display.serial, None);
    }

    #[test]
    fn md5_digests() {
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            md5_hex(b"The quick brown fox jumps over the lazy dog"),
            "9e107d9d372bb6826bd81d3542a419d6"
        );
        let long: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();
        assert_eq!(md5_hex(&long), "a24f1e3ef66950e1327f210e3997ba2c");
        assert_eq!(
            md5_hex(
                b"12345678901234567890123456789012345678901234567890123456789012345678901234567890"
            ),
            "57edf4a22be3c955ac49da2e2107b67a"
        );
    }

    #[test]
    fn icc_files() {
        assert!(importable_name("Monitor (D65).icc"));
        assert!(importable_name("profile"));
        assert!(!importable_name(".hidden.icc"));
        assert!(!importable_name("backup.icc~"));
        assert!(!importable_name("edid-0123456789abcdef.icc"));

        let mut header = vec![0u8; 128];
        header[..4].copy_from_slice(&128u32.to_be_bytes());
        header[36..40].copy_from_slice(b"acsp");
        assert!(is_icc(&header));
        assert!(!is_incomplete(&header));
        assert!(is_incomplete(&header[..100]));
        assert!(!is_icc(b"acsp"));
        assert!(!is_icc(&[0u8; 37]));
        header[36..40].copy_from_slice(b"nope");
        assert!(!is_icc(&header));
    }

    #[test]
    fn file_arguments_escape_what_sway_and_wordexp_read() {
        assert_eq!(
            sway_file_argument("/usr/share/a.icc"),
            "\"/usr/share/a.icc\""
        );
        assert_eq!(
            sway_file_argument("/p/it's a \"screen\"\\"),
            r#""/p/it\'s\ a\ \"screen\"\\""#
        );
        assert_eq!(
            sway_file_argument("/p/$HOME;a,b(c){d}&e|f#g"),
            r#""/p/\$HOME\;a\,b\(c\)\{d\}\&e\|f\#g""#
        );
        assert_eq!(sway_file_argument("/p/Büro.icc"), "\"/p/Büro.icc\"");
        // sway would replace the variable after `\\$`, but turns `$$` into `$`.
        assert_eq!(sway_file_argument("/p/a\\$mod"), r#""/p/a\\\$$mod""#);
        assert_eq!(
            sway_command("DP-1", &Rendering::Icc("/a b".into())),
            r#"output "DP-1" color_profile icc "/a\ b""#
        );
        assert_eq!(
            sway_command("DP-1", &Rendering::Srgb),
            "output \"DP-1\" color_profile srgb"
        );
    }
}
