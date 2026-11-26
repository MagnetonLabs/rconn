//! GNOME on Wayland: the screen size through Mutter's `org.gnome.Mutter.DisplayConfig`
//! D-Bus API, the one GNOME Settings uses. The change is applied as *temporary* (method
//! 1): not saved, and no "Keep these display settings?" question; the layout from before
//! is put back when the session ends.
//!
//! Mutter (46..=48 at least) ends the screen cast of a monitor whose size changes: the
//! portal session is then reopened with its restore token, without asking (see
//! `rconn_media::portal::screen_changed`).
//!
//! The choice of mode is plain logic on [`State`] (tested everywhere); the D-Bus calls
//! are Linux only.

#![cfg_attr(not(target_os = "linux"), allow(dead_code))]

use super::{Mode, pick_fit, pick_restore};

/// One display mode of a monitor, as `GetCurrentState` lists it.
#[derive(Clone, Debug, PartialEq)]
pub struct MonitorMode {
    pub id: String,
    pub width: i32,
    pub height: i32,
    pub refresh: f64,
    pub preferred_scale: f64,
    pub scales: Vec<f64>,
    pub current: bool,
    pub preferred: bool,
}

impl MonitorMode {
    fn mode(&self) -> Mode {
        Mode {
            width: self.width.max(0) as u32,
            height: self.height.max(0) as u32,
            hz: self.refresh,
        }
    }

    /// `scale` if this mode supports it, else the mode's preferred scale.
    fn scale_like(&self, scale: f64) -> f64 {
        if self.scales.iter().any(|s| (s - scale).abs() < 1e-3) {
            scale
        } else {
            self.preferred_scale
        }
    }
}

/// A physical monitor (by connector) and its modes.
#[derive(Clone, Debug, PartialEq)]
pub struct Monitor {
    pub connector: String,
    pub modes: Vec<MonitorMode>,
}

impl Monitor {
    fn current(&self) -> Option<&MonitorMode> {
        self.modes.iter().find(|m| m.current)
    }

    fn as_modes(&self) -> Vec<Mode> {
        self.modes.iter().map(MonitorMode::mode).collect()
    }
}

/// A region of the desktop shown on one monitor (or several, mirrored).
#[derive(Clone, Debug, PartialEq)]
pub struct LogicalMonitor {
    pub x: i32,
    pub y: i32,
    pub scale: f64,
    /// 0 normal, 1 90°, 2 180°, 3 270°, 4..=7 the same flipped.
    pub transform: u32,
    pub primary: bool,
    pub connectors: Vec<String>,
}

/// What `GetCurrentState` reports (the parts used here).
#[derive(Clone, Debug, PartialEq)]
pub struct State {
    pub serial: u32,
    pub monitors: Vec<Monitor>,
    pub logical: Vec<LogicalMonitor>,
}

/// A logical monitor as `ApplyMonitorsConfig` takes it: its monitors as (connector,
/// mode id).
#[derive(Clone, Debug, PartialEq)]
pub struct LogicalConfig {
    pub x: i32,
    pub y: i32,
    pub scale: f64,
    pub transform: u32,
    pub primary: bool,
    pub monitors: Vec<(String, String)>,
}

impl State {
    fn monitor(&self, connector: &str) -> Option<&Monitor> {
        self.monitors.iter().find(|m| m.connector == connector)
    }

    /// The layout as it is now, in the form `ApplyMonitorsConfig` takes.
    pub fn config(&self) -> Result<Vec<LogicalConfig>, String> {
        self.logical
            .iter()
            .map(|l| {
                let monitors = l
                    .connectors
                    .iter()
                    .map(|c| {
                        let mode = self
                            .monitor(c)
                            .and_then(Monitor::current)
                            .ok_or_else(|| format!("monitor {c} has no current mode"))?;
                        Ok((c.clone(), mode.id.clone()))
                    })
                    .collect::<Result<_, String>>()?;
                Ok(LogicalConfig {
                    x: l.x,
                    y: l.y,
                    scale: l.scale,
                    transform: l.transform,
                    primary: l.primary,
                    monitors,
                })
            })
            .collect()
    }
}

/// A layout to apply, and the size the fitted monitor then shows.
pub type Fitted = (Vec<LogicalConfig>, (u32, u32));

/// The layout with the primary monitor switched to the mode closest to `want`; `None`
/// if it already has that size.
pub fn fit_config(state: &State, want: (u32, u32)) -> Result<Option<Fitted>, String> {
    let mut config = state.config()?;
    let i = state.logical.iter().position(|l| l.primary).unwrap_or(0);
    let (Some(logical), Some(target)) = (state.logical.get(i), config.get_mut(i)) else {
        return Err("no monitor is on".into());
    };
    let [connector] = logical.connectors.as_slice() else {
        return Err("mirrored displays are not fitted".into());
    };
    let monitor = state
        .monitor(connector)
        .ok_or_else(|| format!("monitor {connector} is not listed"))?;
    let current = monitor
        .current()
        .ok_or_else(|| format!("monitor {connector} has no current mode"))?;
    // Modes are listed unrotated; a monitor turned on its side shows them the other way.
    let rotated = logical.transform % 2 == 1;
    let turn = |(w, h): (u32, u32)| if rotated { (h, w) } else { (w, h) };
    let chosen = pick_fit(&monitor.as_modes(), turn(want), current.refresh)
        .map(|j| &monitor.modes[j])
        .ok_or_else(|| format!("monitor {connector} lists no modes"))?;
    if (chosen.width, chosen.height) == (current.width, current.height) {
        return Ok(None);
    }
    target.monitors[0].1 = chosen.id.clone();
    target.scale = chosen.scale_like(logical.scale);
    let size = turn((chosen.width.max(0) as u32, chosen.height.max(0) as u32));
    Ok(Some((config, size)))
}

/// The layout that brings back `before` (taken at the first fit) on the monitors as they
/// are `now`: the same modes where they are still listed, else the closest (see
/// [`pick_restore`]).
pub fn restore_config(now: &State, before: &State) -> Result<Vec<LogicalConfig>, String> {
    let mut config = before.config()?;
    for logical in &mut config {
        for (connector, id) in &mut logical.monitors {
            let monitor = now
                .monitor(connector)
                .ok_or_else(|| format!("monitor {connector} is gone"))?;
            if monitor.modes.iter().any(|m| m.id == *id) {
                continue;
            }
            let original = before
                .monitor(connector)
                .and_then(|m| m.modes.iter().find(|m| m.id == *id))
                .ok_or_else(|| format!("mode {id} of {connector} is not listed"))?;
            let preferred = monitor.modes.iter().position(|m| m.preferred);
            let j = pick_restore(&monitor.as_modes(), original.mode(), preferred)
                .ok_or_else(|| format!("monitor {connector} lists no modes"))?;
            let mode = &monitor.modes[j];
            *id = mode.id.clone();
            logical.scale = mode.scale_like(logical.scale);
        }
    }
    Ok(config)
}

#[cfg(target_os = "linux")]
pub use dbus::{restore, set};

#[cfg(target_os = "linux")]
mod dbus {
    use super::{LogicalConfig, LogicalMonitor, Monitor, MonitorMode, State};
    use std::collections::HashMap;
    use std::sync::Mutex;
    use std::time::Duration;
    use zbus::{Connection, OwnedVariant, Variant};

    const DEST: &str = "org.gnome.Mutter.DisplayConfig";
    const PATH: &str = "/org/gnome/Mutter/DisplayConfig";
    /// `ApplyMonitorsConfig` method: applied now, not saved, no confirmation dialog.
    const TEMPORARY: u32 = 1;
    const TIMEOUT: Duration = Duration::from_secs(5);

    type Props = HashMap<String, OwnedVariant>;
    type Spec = (String, String, String, String);
    type RawMode = (String, i32, i32, f64, f64, Vec<f64>, Props);
    type RawMonitor = (Spec, Vec<RawMode>, Props);
    type RawLogical = (i32, i32, f64, u32, bool, Vec<Spec>, Props);
    type RawState = (u32, Vec<RawMonitor>, Vec<RawLogical>, Props);
    type ApplyMonitor = (String, String, HashMap<String, Variant<'static>>);
    type ApplyLogical = (i32, i32, f64, u32, bool, Vec<ApplyMonitor>);

    /// The layout before this host's first change (put back at the end).
    static BEFORE: Mutex<Option<State>> = Mutex::new(None);

    fn flag(props: &Props, key: &str) -> bool {
        props
            .get(key)
            .and_then(|v| bool::try_from(v).ok())
            .unwrap_or(false)
    }

    fn parse((serial, monitors, logical, _): RawState) -> State {
        State {
            serial,
            monitors: monitors
                .into_iter()
                .map(|((connector, ..), modes, _)| Monitor {
                    connector,
                    modes: modes
                        .into_iter()
                        .map(
                            |(id, width, height, refresh, preferred_scale, scales, props)| {
                                MonitorMode {
                                    current: flag(&props, "is-current"),
                                    preferred: flag(&props, "is-preferred"),
                                    id,
                                    width,
                                    height,
                                    refresh,
                                    preferred_scale,
                                    scales,
                                }
                            },
                        )
                        .collect(),
                })
                .collect(),
            logical: logical
                .into_iter()
                .map(
                    |(x, y, scale, transform, primary, specs, _)| LogicalMonitor {
                        x,
                        y,
                        scale,
                        transform,
                        primary,
                        connectors: specs.into_iter().map(|(c, ..)| c).collect(),
                    },
                )
                .collect(),
        }
    }

    async fn current_state(c: &Connection) -> Result<State, String> {
        let reply = c
            .call_method(Some(DEST), PATH, Some(DEST), "GetCurrentState", &())
            .await
            .map_err(|e| format!("GetCurrentState: {e}"))?;
        let raw: RawState = reply
            .body()
            .deserialize()
            .map_err(|e| format!("GetCurrentState reply: {e}"))?;
        Ok(parse(raw))
    }

    async fn apply(c: &Connection, serial: u32, config: &[LogicalConfig]) -> Result<(), String> {
        let logical: Vec<ApplyLogical> = config
            .iter()
            .map(|l| {
                let monitors = l
                    .monitors
                    .iter()
                    .map(|(c, m)| (c.clone(), m.clone(), HashMap::new()))
                    .collect();
                (l.x, l.y, l.scale, l.transform, l.primary, monitors)
            })
            .collect();
        let props: HashMap<String, Variant<'static>> = HashMap::new();
        // GNOME ends the screen cast of a monitor whose size changes: the capture reopens
        // the portal session (silently, with its restore token).
        rconn_media::portal::expect_screen_change();
        c.call_method(
            Some(DEST),
            PATH,
            Some(DEST),
            "ApplyMonitorsConfig",
            &(serial, TEMPORARY, logical, props),
        )
        .await
        .map_err(|e| format!("ApplyMonitorsConfig: {e}"))?;
        rconn_media::portal::screen_changed();
        Ok(())
    }

    /// Run D-Bus calls on a thread of their own (callers may or may not be inside tokio).
    fn run<T: Send, F: Future<Output = Result<T, String>>>(
        f: impl FnOnce(Connection) -> F + Send,
    ) -> Result<T, String> {
        std::thread::scope(|s| {
            s.spawn(|| {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|e| format!("runtime: {e}"))?;
                rt.block_on(async {
                    tokio::time::timeout(TIMEOUT, async {
                        let c = Connection::session()
                            .await
                            .map_err(|e| format!("session bus: {e}"))?;
                        f(c).await
                    })
                    .await
                    .map_err(|_| "Mutter did not answer".to_owned())?
                })
            })
            .join()
            .unwrap_or_else(|_| Err("display thread panicked".into()))
        })
    }

    pub fn set(width: u32, height: u32) -> Result<Option<(u32, u32)>, String> {
        run(|c| async move {
            let state = current_state(&c)
                .await
                .map_err(|e| format!("{e} (not GNOME?)"))?;
            let Some((config, size)) = super::fit_config(&state, (width, height))? else {
                return Ok(None);
            };
            apply(&c, state.serial, &config).await?;
            BEFORE.lock().expect("display state").get_or_insert(state);
            Ok(Some(size))
        })
    }

    pub fn restore() {
        let Some(before) = BEFORE.lock().expect("display state").take() else {
            return;
        };
        let r = run(|c| async move {
            let now = current_state(&c).await?;
            let config = super::restore_config(&now, &before)?;
            if now.config().is_ok_and(|current| current == config) {
                return Ok(());
            }
            apply(&c, now.serial, &config).await
        });
        if let Err(e) = r {
            tracing::warn!(error = %e, "could not put the screen size back");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(id: &str, width: i32, height: i32, scales: &[f64]) -> MonitorMode {
        MonitorMode {
            id: id.into(),
            width,
            height,
            refresh: 60.0,
            preferred_scale: 1.0,
            scales: scales.to_vec(),
            current: false,
            preferred: false,
        }
    }

    /// The Ubuntu 24.04 VM under Parallels (abridged `GetCurrentState`).
    fn vm() -> State {
        let mut modes = vec![
            mode("4096x2160@60.000", 4096, 2160, &[1.0, 2.0, 3.0, 4.0]),
            mode("2560x1600@59.987", 2560, 1600, &[1.0, 2.0, 3.0]),
            mode("1920x1200@59.950", 1920, 1200, &[1.0, 2.0]),
            mode("1920x1080@60.000", 1920, 1080, &[1.0, 2.0]),
            mode("1920x1012@59.981", 1920, 1012, &[1.0, 2.0]),
            mode("1600x900@60.000", 1600, 900, &[1.0]),
            mode("1280x800@59.910", 1280, 800, &[1.0]),
        ];
        modes[4].current = true;
        modes[4].preferred = true;
        State {
            serial: 7,
            monitors: vec![Monitor {
                connector: "Virtual-1".into(),
                modes,
            }],
            logical: vec![LogicalMonitor {
                x: 0,
                y: 0,
                scale: 2.0,
                transform: 0,
                primary: true,
                connectors: vec!["Virtual-1".into()],
            }],
        }
    }

    #[test]
    fn fits_the_primary_monitor_keeping_its_scale_when_possible() {
        let s = vm();
        let (config, size) = fit_config(&s, (1900, 1190)).unwrap().unwrap();
        assert_eq!(size, (1920, 1200));
        assert_eq!(config.len(), 1);
        assert_eq!(
            config[0].monitors,
            [("Virtual-1".into(), "1920x1200@59.950".into())]
        );
        assert_eq!((config[0].x, config[0].y, config[0].primary), (0, 0, true));
        assert_eq!(config[0].scale, 2.0);
        // 1600x900 has no 2x scale: its preferred one.
        let (config, size) = fit_config(&s, (1600, 900)).unwrap().unwrap();
        assert_eq!(size, (1600, 900));
        assert_eq!(config[0].scale, 1.0);
        // Already that size.
        assert_eq!(fit_config(&s, (1920, 1012)).unwrap(), None);
    }

    #[test]
    fn a_monitor_on_its_side_fits_a_tall_window() {
        let mut s = vm();
        s.logical[0].transform = 1;
        let (config, size) = fit_config(&s, (1200, 1900)).unwrap().unwrap();
        assert_eq!(size, (1200, 1920));
        assert_eq!(config[0].monitors[0].1, "1920x1200@59.950");
    }

    #[test]
    fn mirrored_or_empty_layouts_are_not_fitted() {
        let mut s = vm();
        s.logical[0].connectors.push("Virtual-2".into());
        assert!(fit_config(&s, (1600, 900)).is_err());
        s.logical.clear();
        assert!(fit_config(&s, (1600, 900)).is_err());
    }

    #[test]
    fn restores_the_original_mode_or_the_closest_one() {
        let before = vm();
        // Still listed: the same mode, the same scale.
        let mut now = vm();
        now.serial = 9;
        now.monitors[0].modes[4].current = false;
        now.monitors[0].modes[2].current = true;
        let config = restore_config(&now, &before).unwrap();
        assert_eq!(config, before.config().unwrap());
        // Parallels replaced its window-sized mode (1920x1012) with the viewer's shape:
        // the original shape comes back through 4096x2160.
        now.monitors[0].modes[4] = mode("1616x1012@59.981", 1616, 1012, &[1.0, 2.0]);
        let config = restore_config(&now, &before).unwrap();
        assert_eq!(config[0].monitors[0].1, "4096x2160@60.000");
        assert_eq!(config[0].scale, 2.0);
        // The monitor was unplugged meanwhile.
        now.monitors[0].connector = "HDMI-1".into();
        assert!(restore_config(&now, &before).is_err());
    }
}
