// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

// A few helper methods on `TabPower` (`label` / `cpu_label` / `watts_label`)
// are GUI-rendering helpers; headless builds compile the type because
// the API snapshot still carries `Vec<TabPower>`, but never invokes
// the formatting helpers.
#![cfg_attr(not(feature = "gui"), allow(dead_code))]

use std::sync::{Arc, Mutex};

use log::info;
use wattaouille::{PowerSensor, num_cpus, sum_tree_jiffies, total_cpu_jiffies};

#[derive(Clone, Default)]
pub struct TabPower {
    pub cpu_percent: f64,
    pub watts: Option<f64>,
}

impl TabPower {
    pub fn label(&self) -> String {
        if let Some(w) = self.watts {
            if w >= 100.0 {
                return format!("{w:.0}W");
            } else if w >= 1.0 {
                return format!("{w:.1}W");
            } else if w >= 0.01 {
                return format!("{:.0}mW", w * 1000.0);
            }
            // Below 10 mW (including ~zero): keep the milliwatt unit so a
            // near-idle tab reads "0mW" in line with its neighbours, not a
            // jarring "0W" sitting next to "13mW".
            return "0mW".into();
        }
        if self.cpu_percent >= 0.1 {
            return self.cpu_label();
        }
        "0%".into()
    }

    pub fn cpu_label(&self) -> String {
        if self.cpu_percent >= 100.0 {
            format!("{:.0}%", self.cpu_percent)
        } else {
            format!("{:.1}%", self.cpu_percent)
        }
    }

    pub fn watts_label(&self) -> String {
        self.watts.map_or_else(String::new, |w| {
            if w >= 100.0 {
                format!("{w:.0} W")
            } else if w >= 1.0 {
                format!("{w:.1} W")
            } else if w >= 0.01 {
                format!("{:.0} mW", w * 1000.0)
            } else {
                String::new()
            }
        })
    }
}

struct PrevState {
    energy_uj: Option<u64>,
    total_jiffies: u64,
    per_tab_jiffies: Vec<u64>,
}

pub struct PowerMonitor {
    sensor: PowerSensor,
    cpus: u64,
    prev: Option<PrevState>,
}

impl PowerMonitor {
    pub fn new() -> Self {
        let sensor = PowerSensor::detect(false);
        let cpus = num_cpus().max(1);
        info!(
            "power monitor: {} CPUs, RAPL {}",
            cpus,
            if sensor.enabled { "available" } else { "unavailable" }
        );
        Self {
            sensor,
            cpus,
            prev: None,
        }
    }

    pub fn sample(&mut self, tab_pids: &[u32], interval_secs: f64) -> Vec<TabPower> {
        let cur_total = total_cpu_jiffies();
        let per_tab_jiffies: Vec<u64> = tab_pids.iter().map(|&pid| sum_tree_jiffies(pid)).collect();
        let energy_uj = self.sensor.read_uj();

        let result = if let Some(ref prev) = self.prev {
            let total_delta = cur_total.saturating_sub(prev.total_jiffies).max(1);
            let frame_joules = match (prev.energy_uj, energy_uj) {
                (Some(before), Some(after)) => self.sensor.joules_between(before, after),
                _ => 0.0,
            };
            let frame_watts = if interval_secs > 0.0 {
                frame_joules / interval_secs
            } else {
                0.0
            };

            per_tab_jiffies
                .iter()
                .zip(prev.per_tab_jiffies.iter())
                .map(|(&cur, &prev_j)| {
                    let delta = cur.saturating_sub(prev_j);
                    let share = delta as f64 / total_delta as f64;
                    let cpu_percent = share * 100.0 * self.cpus as f64;
                    let watts = if self.sensor.enabled {
                        Some(frame_watts * share)
                    } else {
                        None
                    };
                    TabPower { cpu_percent, watts }
                })
                .collect()
        } else {
            tab_pids.iter().map(|_| TabPower::default()).collect()
        };

        self.prev = Some(PrevState {
            energy_uj,
            total_jiffies: cur_total,
            per_tab_jiffies,
        });

        result
    }
}

pub fn read_battery_percent() -> Option<u8> {
    let dir = std::fs::read_dir("/sys/class/power_supply").ok()?;
    for entry in dir.flatten() {
        let name = entry.file_name();
        if !name.to_string_lossy().starts_with("BAT") {
            continue;
        }
        let status = std::fs::read_to_string(entry.path().join("status")).unwrap_or_default();
        if status.trim() != "Discharging" {
            continue;
        }
        let capacity = std::fs::read_to_string(entry.path().join("capacity")).ok()?;
        return capacity.trim().parse().ok();
    }
    None
}

pub fn start_power_monitor(
    tab_pids: Arc<Mutex<Vec<u32>>>,
    results: Arc<Mutex<Vec<TabPower>>>,
    battery: Arc<Mutex<Option<u8>>>,
    hot: Arc<std::sync::atomic::AtomicBool>,
) {
    std::thread::spawn(move || {
        let mut monitor = PowerMonitor::new();
        // Fast cadence while someone can SEE the numbers (window visible /
        // API consumer active — the owner flips `hot`); 5× slower
        // otherwise. The sample itself walks every tab's whole /proc
        // subtree (one stat + children read per descendant — ~hundreds of
        // opens with a fleet of agent tabs), which is a lot of steady
        // background syscalls for a hidden display. Sampling never stops
        // entirely: the energy accounting integrates these watts, and
        // agents keep drawing power while the window is hidden — the
        // jiffies/energy counters are cumulative, so a longer window
        // loses no energy, just per-tab display granularity.
        let step = std::time::Duration::from_secs(2);
        let cold_every = 5u32; // 10 s cadence when nobody is looking
        let mut cold_ctr = 0u32;
        let mut last_sample = std::time::Instant::now();
        loop {
            std::thread::sleep(step);
            if !hot.load(std::sync::atomic::Ordering::Relaxed) {
                cold_ctr += 1;
                if cold_ctr < cold_every {
                    continue;
                }
            }
            cold_ctr = 0;
            *battery.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = read_battery_percent();
            let pids = tab_pids
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let elapsed = last_sample.elapsed().as_secs_f64();
            last_sample = std::time::Instant::now();
            if pids.is_empty() {
                continue;
            }
            // Real elapsed time, not the nominal interval — the watts
            // figure would otherwise be 5× off on the first hot sample
            // after a cold stretch.
            let snapshot = monitor.sample(&pids, elapsed);
            *results.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = snapshot;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_sample_is_zero() {
        let mut m = PowerMonitor {
            sensor: PowerSensor::detect(true),
            cpus: 4,
            prev: None,
        };
        let snap = m.sample(&[1, 2], 2.0);
        assert_eq!(snap.len(), 2);
        assert!((snap[0].cpu_percent - 0.0).abs() < f64::EPSILON);
        assert!(snap[0].watts.is_none());
    }

    #[test]
    fn label_shows_percent_without_rapl() {
        let tp = TabPower {
            cpu_percent: 12.3,
            watts: None,
        };
        assert_eq!(tp.label(), "12.3%");
    }

    #[test]
    fn label_shows_watts_when_high() {
        let tp = TabPower {
            cpu_percent: 50.0,
            watts: Some(3.5),
        };
        assert_eq!(tp.label(), "3.5W");
    }

    #[test]
    fn label_shows_milliwatts() {
        let tp = TabPower {
            cpu_percent: 1.0,
            watts: Some(0.05),
        };
        assert_eq!(tp.label(), "50mW");
    }

    #[test]
    fn label_shows_zero_milliwatts_when_tiny() {
        // Sub-10mW (incl. ~zero) stays in mW so it lines up with
        // neighbouring tabs' "13mW" rather than showing a stray "0W".
        let tp = TabPower {
            cpu_percent: 0.5,
            watts: Some(0.001),
        };
        assert_eq!(tp.label(), "0mW");
        // Exactly zero too.
        let tp_zero = TabPower {
            cpu_percent: 0.0,
            watts: Some(0.0),
        };
        assert_eq!(tp_zero.label(), "0mW");
    }

    #[test]
    fn label_shows_zero_when_idle() {
        let tp = TabPower {
            cpu_percent: 0.0,
            watts: None,
        };
        assert_eq!(tp.label(), "0%");
    }

    #[test]
    fn watts_label_formats_correctly() {
        assert_eq!(
            TabPower {
                cpu_percent: 50.0,
                watts: Some(3.5)
            }
            .watts_label(),
            "3.5 W"
        );
        assert_eq!(
            TabPower {
                cpu_percent: 1.0,
                watts: Some(0.05)
            }
            .watts_label(),
            "50 mW"
        );
        assert_eq!(
            TabPower {
                cpu_percent: 0.0,
                watts: None
            }
            .watts_label(),
            ""
        );
    }

    #[test]
    fn watts_label_high_wattage() {
        let tp = TabPower {
            cpu_percent: 90.0,
            watts: Some(150.0),
        };
        assert_eq!(tp.watts_label(), "150 W");
    }

    #[test]
    fn watts_label_tiny_returns_empty() {
        let tp = TabPower {
            cpu_percent: 0.0,
            watts: Some(0.005),
        };
        assert_eq!(tp.watts_label(), "");
    }

    #[test]
    fn cpu_label_under_100() {
        let tp = TabPower {
            cpu_percent: 45.7,
            watts: None,
        };
        assert_eq!(tp.cpu_label(), "45.7%");
    }

    #[test]
    fn cpu_label_over_100() {
        let tp = TabPower {
            cpu_percent: 312.0,
            watts: None,
        };
        assert_eq!(tp.cpu_label(), "312%");
    }

    #[test]
    fn label_high_watts() {
        let tp = TabPower {
            cpu_percent: 80.0,
            watts: Some(200.0),
        };
        assert_eq!(tp.label(), "200W");
    }

    #[test]
    fn second_sample_has_values() {
        let mut m = PowerMonitor {
            sensor: PowerSensor::detect(true),
            cpus: 4,
            prev: None,
        };
        let pid = std::process::id();
        m.sample(&[pid], 1.0);
        let snap = m.sample(&[pid], 1.0);
        assert_eq!(snap.len(), 1);
        assert!(snap[0].cpu_percent >= 0.0);
    }
}
