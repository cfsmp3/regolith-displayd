//! Output mirroring. Regolith's sway shows one output on another with
//! `output <name> mirror <source>`, and ends it with `output <name> mirror
//! off`. While it mirrors, an output takes no room in the layout and has no
//! workspaces, and `get_outputs` names its source in a `mirror` entry.
//!
//! swayipc's `Output` has no field for that entry, so the outputs are read
//! here with it (see [`get_outputs`]). A sway that cannot mirror outputs
//! reports no such entry at all.
//!
//! kanshi cannot mirror outputs. A kept profile records the outputs that
//! mirror others in comments, and runs `regolith-displayd --restore-mirrors`
//! (see [`RESTORE_ARG`]) once kanshi has applied it.

use serde_json::Value;
use std::{error::Error, path::PathBuf};
use swayipc_async::{CommandType, Output, MAGIC};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// An error that may be held across an await in the daemon's tasks.
pub type BoxError = Box<dyn Error + Send + Sync>;

/// The argument that runs the binary as the helper that mirrors the outputs,
/// and only those, as the profile of the connected monitors records it (see
/// `restore_saved_mirrors`).
pub const RESTORE_ARG: &str = "--restore-mirrors";

/// A sway output, with the output it mirrors.
#[derive(Debug)]
pub struct SwayOutput {
    pub output: Output,
    /// The connector in the output's `mirror` entry, if any.
    pub mirror: Option<String>,
    /// Whether sway reported a `mirror` entry at all: only a sway that can
    /// mirror outputs does.
    pub reports_mirror: bool,
}

impl From<Output> for SwayOutput {
    fn from(output: Output) -> SwayOutput {
        SwayOutput {
            output,
            mirror: None,
            reports_mirror: false,
        }
    }
}

impl SwayOutput {
    /// The connector of the output this one shows, among sway's `outputs`:
    /// the output in its `mirror` entry while both are on and that output
    /// shows itself, else None. sway only mirrors in that case: otherwise
    /// the output shows its own part of the layout.
    pub fn source<'a>(&self, outputs: &'a [SwayOutput]) -> Option<&'a str> {
        if !self.output.active {
            return None;
        }
        let name = self.mirror.as_deref()?;
        outputs
            .iter()
            .find(|other| other.output.name == name)
            .filter(|other| other.output.active && other.mirror.is_none())
            .map(|other| other.output.name.as_str())
    }
}

/// Whether the sway that reported `outputs` can mirror outputs.
pub fn can_mirror(outputs: &[SwayOutput]) -> bool {
    outputs.iter().any(|output| output.reports_mirror)
}

/// The path of sway's IPC socket, found as swayipc finds it.
async fn socket_path() -> Result<PathBuf, BoxError> {
    for variable in ["I3SOCK", "SWAYSOCK"] {
        if let Some(path) = std::env::var_os(variable) {
            return Ok(path.into());
        }
    }
    let output = tokio::process::Command::new("sway")
        .arg("--get-socketpath")
        .output()
        .await?;
    let path = String::from_utf8(output.stdout)?.trim_end().to_string();
    if path.is_empty() {
        return Err("Cannot find sway's IPC socket".into());
    }
    Ok(path.into())
}

/// sway's outputs, as `get_outputs` reports them, with the output each one
/// mirrors.
pub async fn get_outputs() -> Result<Vec<SwayOutput>, BoxError> {
    let mut stream = UnixStream::connect(socket_path().await?).await?;
    stream.write_all(&CommandType::GetOutputs.encode()).await?;
    // The magic string, then the payload's length and type in native byte
    // order, as swayipc reads them.
    let mut header = [0u8; 14];
    stream.read_exact(&mut header).await?;
    if header[..6] != MAGIC {
        return Err("Unexpected reply from sway".into());
    }
    let length = u32::from_ne_bytes([header[6], header[7], header[8], header[9]]);
    let reply_type = u32::from_ne_bytes([header[10], header[11], header[12], header[13]]);
    let mut payload = vec![0u8; length as usize];
    stream.read_exact(&mut payload).await?;
    let values: Vec<Value> = CommandType::GetOutputs.decode((reply_type, payload))?;
    parse_outputs(values)
}

/// The outputs of a `get_outputs` reply.
pub fn parse_outputs(values: Vec<Value>) -> Result<Vec<SwayOutput>, BoxError> {
    values
        .into_iter()
        .map(|value| {
            let reports_mirror = value.get("mirror").is_some();
            let mirror = value
                .get("mirror")
                .and_then(Value::as_str)
                .map(str::to_string);
            Ok(SwayOutput {
                output: serde_json::from_value(value)?,
                mirror,
                reports_mirror,
            })
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The outputs of a `get_outputs` reply, given as JSON.
    pub(crate) fn from_reply(reply: serde_json::Value) -> Vec<SwayOutput> {
        parse_outputs(serde_json::from_value(reply).unwrap()).unwrap()
    }

    /// An active 1920x1080 output at `x`, mirroring `mirror` if given: the
    /// way sway reports a mirror, with its source's rectangle.
    pub(crate) fn output(name: &str, x: i32, mirror: Option<&str>) -> serde_json::Value {
        serde_json::json!({
            "id": 1, "name": name, "make": "Acme", "model": name, "serial": "1",
            "active": true, "dpms": true, "primary": false, "scale": 1.0,
            "transform": "normal", "modes": [], "mirror": mirror,
            "current_mode": {"width": 1920, "height": 1080, "refresh": 60000},
            "rect": {"x": x, "y": 0, "width": 1920, "height": 1080}
        })
    }

    #[test]
    fn mirror_entries_are_read() {
        let outputs = from_reply(serde_json::json!([
            output("DP-1", 0, None),
            output("HDMI-A-1", 0, Some("DP-1")),
        ]));
        assert!(can_mirror(&outputs));
        assert_eq!(outputs[0].mirror, None);
        assert_eq!(outputs[1].mirror.as_deref(), Some("DP-1"));
        assert_eq!(outputs[0].source(&outputs), None);
        assert_eq!(outputs[1].source(&outputs), Some("DP-1"));
    }

    #[test]
    fn without_mirror_entries_sway_cannot_mirror() {
        let mut reply = [output("DP-1", 0, None), output("DP-2", 1920, None)];
        for output in &mut reply {
            output.as_object_mut().unwrap().remove("mirror");
        }
        let outputs = from_reply(serde_json::json!(reply));
        assert!(!can_mirror(&outputs));
        assert!(outputs
            .iter()
            .all(|output| output.source(&outputs).is_none()));
    }

    #[test]
    fn a_mirror_whose_source_is_gone_shows_itself() {
        let mut source = output("DP-1", 0, None);
        source["active"] = serde_json::json!(false);
        let outputs = from_reply(serde_json::json!([source, output("DP-2", 0, Some("DP-1"))]));
        assert_eq!(outputs[1].source(&outputs), None);
        let unplugged = from_reply(serde_json::json!([output("DP-2", 0, Some("DP-1"))]));
        assert_eq!(unplugged[0].source(&unplugged), None);
    }
}
