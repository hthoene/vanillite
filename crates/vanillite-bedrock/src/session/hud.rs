//! Java packets without a direct Bedrock counterpart: advancement toasts, the world border (a
//! particle wall near the player, as Bedrock has no border), `stop_sound`, `hurt_animation` and
//! `transfer`.

use std::time::{Duration, Instant};

use rustc_hash::FxHashMap;
use vanillite_protocol::codec::{Decode, Reader as JReader};
use vanillite_protocol::packets::play::clientbound as pcb;
use vanillite_protocol::text::Component;

use super::Session;
use crate::packets::{self as bp, id};

/// Most advancements with toasts remembered (Vanilla has about 120).
const MAX_TOASTS: usize = 4096;

/// Distance to the border at which the wall shows at least (`WorldBorder` warning blocks are
/// added on top).
const CLOSE_TO_BORDER: f64 = 5.0;

/// How often the wall is drawn.
const WALL_EVERY: Duration = Duration::from_millis(200);

/// Bedrock `level_event` of the red "deny" particle.
const PARTICLE_DENY_BLOCK: i32 = 2008;

/// An advancement that shows a toast when it is done.
#[derive(Debug, Clone)]
pub(super) struct Toast {
    title: Component,
    frame: i32,
    requirements: Vec<Vec<String>>,
}

/// The world border as the Java client tracks it.
#[derive(Debug, Clone)]
pub(super) struct Border {
    center: [f64; 2],
    from: f64,
    to: f64,
    lerp_start: Instant,
    lerp: Duration,
    warning_blocks: f64,
    absolute_max: f64,
    /// When the wall was last drawn (at most every [`WALL_EVERY`], however fast the client
    /// sends movement).
    drawn: Instant,
}

impl Default for Border {
    fn default() -> Self {
        Self {
            center: [0.0; 2],
            from: 59_999_968.0,
            to: 59_999_968.0,
            lerp_start: Instant::now(),
            lerp: Duration::ZERO,
            warning_blocks: 5.0,
            absolute_max: 29_999_984.0,
            drawn: Instant::now(),
        }
    }
}

impl Border {
    fn size(&self, now: Instant) -> f64 {
        let elapsed = now.saturating_duration_since(self.lerp_start);
        if self.lerp.is_zero() || elapsed >= self.lerp {
            return self.to;
        }
        let t = elapsed.as_secs_f64() / self.lerp.as_secs_f64();
        self.from + (self.to - self.from) * t
    }

    fn set_lerp(&mut self, from: f64, to: f64, millis: i64) {
        self.from = from;
        self.to = to;
        self.lerp_start = Instant::now();
        self.lerp = Duration::from_millis(u64::try_from(millis).unwrap_or(0));
    }

    /// `(min x, min z, max x, max z)`.
    fn bounds(&self, now: Instant) -> [f64; 4] {
        let r = self.size(now) / 2.0;
        let m = self.absolute_max;
        [
            (self.center[0] - r).clamp(-m, m),
            (self.center[1] - r).clamp(-m, m),
            (self.center[0] + r).clamp(-m, m),
            (self.center[1] + r).clamp(-m, m),
        ]
    }
}

/// Half the width of a player (the border stops its bounding box).
const HALF_WIDTH: f64 = 0.3;

/// Where a move from `from` to `to` ends at the border: `to` with x and z clamped when the
/// player was inside and would leave (Java's border collides only with entities inside it, so
/// a player outside after the border shrank may walk freely). `None` when the move is free.
fn clamp_move(bounds: [f64; 4], from: [f64; 3], to: [f64; 3]) -> Option<[f64; 3]> {
    let [min_x, min_z, max_x, max_z] = bounds;
    let (lo_x, hi_x) = (min_x + HALF_WIDTH, max_x - HALF_WIDTH);
    let (lo_z, hi_z) = (min_z + HALF_WIDTH, max_z - HALF_WIDTH);
    // A little slack: the last position may already be clamped.
    const SLACK: f64 = 1e-3;
    let inside = from[0] >= lo_x - SLACK
        && from[0] <= hi_x + SLACK
        && from[2] >= lo_z - SLACK
        && from[2] <= hi_z + SLACK;
    if !inside || lo_x > hi_x || lo_z > hi_z {
        return None;
    }
    let x = to[0].clamp(lo_x, hi_x);
    let z = to[2].clamp(lo_z, hi_z);
    (x != to[0] || z != to[2]).then_some([x, to[1], z])
}

/// The points of the wall near `pos` (6 wide, 5 high) on the sides the player is close to.
fn wall(bounds: [f64; 4], warning: f64, pos: [f64; 3]) -> Vec<[f32; 3]> {
    let [min_x, min_z, max_x, max_z] = bounds;
    let near = warning.max(CLOSE_TO_BORDER);
    let mut points = Vec::new();
    let f = crate::num::f32_of;
    let base_y = pos[1].floor() - 1.0;
    let mut side = |fixed: f64, along_x: bool| {
        let around = if along_x { pos[0] } else { pos[2] };
        let (lo, hi) = if along_x {
            (min_x, max_x)
        } else {
            (min_z, max_z)
        };
        for dy in 0..5 {
            for d in -3..3 {
                let a = around.floor() + f64::from(d);
                if a < lo || a > hi {
                    continue;
                }
                let y = f(base_y + f64::from(dy));
                points.push(if along_x {
                    [f(a), y, f(fixed)]
                } else {
                    [f(fixed), y, f(a)]
                });
            }
        }
    };
    if pos[0] > max_x - near {
        side(max_x, false);
    }
    if pos[0] < min_x + near {
        side(min_x, false);
    }
    if pos[2] > max_z - near {
        side(max_z, true);
    }
    if pos[2] < min_z + near {
        side(min_z, true);
    }
    points
}

/// `AdvancementProgress.isDone`: every requirement has a done criterion.
fn done(requirements: &[Vec<String>], criteria: &[(String, Option<i64>)]) -> bool {
    !requirements.is_empty()
        && requirements.iter().all(|any| {
            any.iter()
                .any(|c| criteria.iter().any(|(n, at)| n == c && at.is_some()))
        })
}

impl Session {
    /// `update_advancements`: a toast for each advancement with a toast that is now done
    /// (`ClientAdvancements.update`).
    pub(super) fn java_advancements(&mut self, a: pcb::UpdateAdvancements) {
        if a.reset {
            self.toasts.clear();
        }
        for removed in &a.removed {
            self.toasts.remove(removed.as_str());
        }
        for e in a.added {
            match e.display {
                Some(d) if d.show_toast && self.toasts.len() < MAX_TOASTS => {
                    self.toasts.insert(
                        e.id.as_str().to_owned(),
                        Toast {
                            title: d.title,
                            frame: d.frame,
                            requirements: e.requirements,
                        },
                    );
                }
                _ => {
                    self.toasts.remove(e.id.as_str());
                }
            }
        }
        if a.reset {
            return;
        }
        let shown: Vec<(String, String)> = a
            .progress
            .iter()
            .filter_map(|p| {
                let t = self.toasts.get(p.id.as_str())?;
                if !done(&t.requirements, &p.criteria) {
                    return None;
                }
                // `AdvancementType`: task and goal green, challenge purple.
                let (key, colour) = match t.frame {
                    1 => ("advancements.toast.challenge", "§5"),
                    2 => ("advancements.toast.goal", "§a"),
                    _ => ("advancements.toast.task", "§a"),
                };
                let heading = self.text(&Component::translatable(key, vec![]));
                Some((format!("{colour}{heading}"), self.text(&t.title)))
            })
            .collect();
        for (title, message) in shown {
            self.send(id::TOAST_REQUEST, &bp::toast_request(&title, &message));
        }
    }

    /// The world border packets.
    pub(super) fn java_border(&mut self, packet: i32, r: &mut JReader<'_>) {
        use vanillite_protocol::ids::play::clientbound as ids;
        let b = &mut self.border;
        match packet {
            ids::INITIALIZE_BORDER => {
                if let Ok(p) = pcb::InitializeBorder::decode(r) {
                    b.center = [p.center_x, p.center_z];
                    b.set_lerp(p.old_size, p.new_size, p.lerp_time);
                    b.absolute_max = f64::from(p.absolute_max_size.max(1));
                    b.warning_blocks = f64::from(p.warning_blocks.max(0));
                }
            }
            ids::SET_BORDER_CENTER => {
                if let Ok(p) = pcb::SetBorderCenter::decode(r) {
                    b.center = [p.center_x, p.center_z];
                }
            }
            ids::SET_BORDER_LERP_SIZE => {
                if let Ok(p) = pcb::SetBorderLerpSize::decode(r) {
                    b.set_lerp(p.old_size, p.new_size, p.lerp_time);
                }
            }
            ids::SET_BORDER_SIZE => {
                if let Ok(p) = pcb::SetBorderSize::decode(r) {
                    b.set_lerp(p.size, p.size, 0);
                }
            }
            ids::SET_BORDER_WARNING_DISTANCE => {
                if let Ok(p) = pcb::SetBorderWarningDistance::decode(r) {
                    b.warning_blocks = f64::from(p.warning_blocks.max(0));
                }
            }
            _ => {}
        }
    }

    /// The border collision of a Bedrock move (Bedrock has no border): the position the player
    /// stops at when the move from `from` to `to` would cross it.
    pub(super) fn border_stop(&self, from: [f64; 3], to: [f64; 3]) -> Option<[f64; 3]> {
        clamp_move(self.border.bounds(Instant::now()), from, to)
    }

    /// The wall near the player, five times a second.
    pub(super) fn draw_border(&mut self) {
        let Some(pos) = self.world.as_ref().map(|w| w.pos) else {
            return;
        };
        let now = Instant::now();
        if now.saturating_duration_since(self.border.drawn) < WALL_EVERY {
            return;
        }
        self.border.drawn = now;
        let bounds = self.border.bounds(now);
        for p in wall(bounds, self.border.warning_blocks, pos) {
            self.send(id::LEVEL_EVENT, &bp::level_event(PARTICLE_DENY_BLOCK, p, 0));
        }
    }

    /// `stop_sound`: by name, or all sounds (Bedrock cannot stop a category alone).
    pub(super) fn java_stop_sound(&mut self, r: &mut JReader<'_>) {
        let Ok(s) = pcb::StopSound::decode(r) else {
            return;
        };
        match s.sound {
            Some(sound) => {
                if let Some(name) = crate::data::sound_name(sound.as_str()) {
                    self.send(id::STOP_SOUND, &bp::stop_sound(name, false));
                }
            }
            None => self.send(id::STOP_SOUND, &bp::stop_sound("", true)),
        }
    }

    /// `transfer`: the client moves to the same host's Bedrock port (a Vanillite server there
    /// runs its Bedrock frontend on the same port as this one).
    pub(super) fn java_transfer(&mut self, r: &mut JReader<'_>) {
        let Ok(t) = pcb::Transfer::decode(r) else {
            return;
        };
        let port = self.shared.config.bind.port();
        log::info!(
            "Bedrock player {} transferred to {}:{port}",
            self.peer,
            t.host
        );
        self.send(id::TRANSFER, &bp::transfer(&t.host, port));
    }
}

/// Ids of `command_suggestion` requests for soft enums (the probe index is added).
const PROBE_IDS: i32 = 0x4000_0000;

/// Most suggestions put into one soft enum.
const MAX_SUGGESTIONS: usize = 512;

impl Session {
    /// Asks the server for the next soft enum's suggestions (one per tick: the server answers
    /// only the last request of a tick).
    pub(super) fn ask_suggestions(&mut self) {
        let Some(p) = self.probes.get(self.next_probe) else {
            return;
        };
        if self.java != super::JavaState::Play {
            return;
        }
        let command = p.line.clone();
        let id = PROBE_IDS.saturating_add(i32::try_from(self.next_probe).unwrap_or(0));
        self.next_probe = self.next_probe.saturating_add(1);
        self.java_send(
            &vanillite_protocol::packets::play::serverbound::CommandSuggestion { id, command },
        );
    }

    /// `command_suggestions`: the values of a soft enum.
    pub(super) fn java_suggestions(&mut self, c: &pcb::CommandSuggestions) {
        let Some(p) =
            c.id.checked_sub(PROBE_IDS)
                .and_then(|i| usize::try_from(i).ok())
                .and_then(|i| self.probes.get(i))
        else {
            return;
        };
        let values: Vec<String> = c
            .suggestions
            .iter()
            .take(MAX_SUGGESTIONS)
            .map(|s| s.text.clone())
            .collect();
        let body = crate::commands::update_soft_enum(&p.name, &values);
        self.send(id::UPDATE_SOFT_ENUM, &body);
    }
}

/// Remembered toasts by advancement id.
pub(super) type Toasts = FxHashMap<String, Toast>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn border_stops_players_inside_only() {
        let b = [-10.0, -10.0, 10.0, 10.0];
        assert_eq!(clamp_move(b, [9.0, 64.0, 0.0], [9.5, 63.0, 1.0]), None);
        assert_eq!(
            clamp_move(b, [9.0, 64.0, 0.0], [10.5, 63.0, 1.0]),
            Some([9.7, 63.0, 1.0])
        );
        assert_eq!(
            clamp_move(b, [0.0, 64.0, -9.6], [0.0, 64.0, -11.0]),
            Some([0.0, 64.0, -9.7])
        );
        // Outside (the border shrank): no collision.
        assert_eq!(clamp_move(b, [12.0, 64.0, 0.0], [13.0, 64.0, 0.0]), None);
    }

    #[test]
    fn toast_only_when_every_requirement_is_done() {
        let req = vec![vec!["a".to_owned(), "b".to_owned()], vec!["c".to_owned()]];
        assert!(!done(&req, &[("a".into(), Some(1)), ("c".into(), None)]));
        assert!(done(&req, &[("b".into(), Some(1)), ("c".into(), Some(2))]));
        assert!(!done(&[], &[]));
    }

    #[test]
    fn wall_shows_near_the_border_only() {
        let b = Border {
            center: [0.0, 0.0],
            from: 20.0,
            to: 20.0,
            ..Border::default()
        };
        let bounds = b.bounds(Instant::now());
        assert_eq!(bounds, [-10.0, -10.0, 10.0, 10.0]);
        assert!(wall(bounds, 5.0, [0.0, 64.0, 0.0]).is_empty());
        let near = wall(bounds, 5.0, [8.0, 64.0, 0.0]);
        assert_eq!(near.len(), 30);
        assert!(near.iter().all(|p| p[0] == 10.0));
    }
}
