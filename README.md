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
* Mirroring (needs a sway that mirrors outputs with `output <name> mirror <source>`, such as Regolith's `sway-regolith`)
* Night Light (needs gnome-settings-daemon's color plugin running in the session and a compositor with `wlr-gamma-control`, such as sway)

# Monitor number labels
While its window is active, the Displays panel asks the shell to show each monitor's number on that monitor (`ShowMonitorLabels` and `HideMonitorLabels` of `org.gnome.Shell`). In the sway session `regolith-powerd` owns `org.gnome.Shell` and passes these calls on to this daemon, which draws the labels. The daemon serves the same two methods, with the same arguments, on its own bus name `org.gnome.Mutter.DisplayConfig` at `/org/regolith/displayd/MonitorLabels`, interface `org.regolith.displayd.MonitorLabels`. This interface is internal to Regolith and may change together with `regolith-powerd`.

# What doesn't work?
* ~~Applying **any** changes whatsoever~~
* ~~Display page doesn't update on monitor change. **Workaround**: switch to some other page from the panel and back or restart `gnome-control-center`~~
* ~~Screen Mirroring~~

# Mirroring
Monitors that mirror another one are reported as one logical monitor, the mirrored monitor first, however the mirroring was set up: in Settings, with `swaymsg` or in sway's configuration. In a logical monitor applied from Settings, the first monitor shows the desktop and the others mirror it. kanshi cannot mirror outputs, so a kept profile records the mirrors in `# regolith-displayd: mirror` comments and runs `regolith-displayd --restore-mirrors` once kanshi has applied it.

# Contributing
Any and all contributions are welcome. Any ideas or suggestions are welcome as well.
