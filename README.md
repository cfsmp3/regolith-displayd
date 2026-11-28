# Build

```
cargo build --release 
```

# Installation
```
cargo install --path .
```

# Running the daemon
Simply run the daemon with the following command:

```
regolith-displayd
```

# Usage
Run the daemon with the command specified above. You can then use `gnome-control-center` or variants of it to manage display settings.

# What works?
* Layout
* Resolution
* Refresh Rate
* Scale
* Hotplugging monitors
* Night Light (needs gnome-settings-daemon's color plugin running in the session and a compositor with `wlr-gamma-control`, such as sway)
* Color profiles (needs colord, and a compositor with per-output color profiles, such as sway 1.11)

# Monitor number labels
While its window is active, the Displays panel asks the shell to show each monitor's number on that monitor (`ShowMonitorLabels` and `HideMonitorLabels` of `org.gnome.Shell`). In the sway session `regolith-powerd` owns `org.gnome.Shell` and passes these calls on to this daemon, which draws the labels. The daemon serves the same two methods, with the same arguments, on its own bus name `org.gnome.Mutter.DisplayConfig` at `/org/regolith/displayd/MonitorLabels`, interface `org.regolith.displayd.MonitorLabels`. This interface is internal to Regolith and may change together with `regolith-powerd`.

# Color profiles
As GNOME's compositor does, the daemon registers every output sway lists with colord, as a display device with the id `xrandr-<make>-<model>-<serial>`, imports the ICC profiles in `$XDG_DATA_HOME/icc` (`~/.local/share/icc`), and renders each output with its device's default profile through sway's `color_profile`, or with sRGB when it has none. So the Color panel of GNOME Settings lists the displays and can add and assign profiles, and a profile assigned with colord's own tools or in a GNOME session applies here too. Two outputs showing the same make, model and serial get separate devices: the one registered first keeps the id, the others get `-<connector>` appended. While a calibration tool inhibits a device (`ProfilingInhibit`), or while the device is disabled (`SetEnabled`), its output is rendered with sRGB. The file of each output's default profile is watched: rewritten, it is loaded again; removed, the output goes back to sRGB until another profile is the default. The profile of an ICC file that is rewritten or removed is replaced or removed too. colord may start after the daemon, or restart: the daemon follows it.

# What doesn't work?
* ~~Applying **any** changes whatsoever~~
* ~~Display page doesn't update on monitor change. **Workaround**: switch to some other page from the panel and back or restart `gnome-control-center`~~
* Screen Mirroring (sway doesn't support it and probably won't for the forseeable future).

# Contributing
Any and all contributions are welcome. Any ideas or suggestions are welcome as well.
