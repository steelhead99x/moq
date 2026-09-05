//! One controller for every generated rung's share, encoder target, and stalled
//! state in a single output bandwidth domain.
//!
//! Catalog `stalled` follows the last target an encoder *accepted*, not the one
//! the controller requested. Without a bandwidth input the controller stays
//! fixed-rate and never publishes congestion-induced stall.

use std::collections::HashMap;
use std::task::Poll;
use std::time::Instant;

use hang::catalog::PRIORITY;
use moq_video::encode::rate::{Control as Rate, Policy};

use crate::catalog::{Published, Resolved};

/// Stall boundary for a rendition with configured maximum `max` and the next
/// lower rendition's configured maximum `lower`.
///
/// The lowest rung uses `lower = 0`, so its boundary is `max / 3`. An encoder
/// may adapt within `[stall, max]`; at the boundary it clamps and the catalog
/// publishes `stalled` until a target above the same boundary is applied.
pub fn stall_boundary(max: u64, lower: u64) -> u64 {
	max.saturating_add(lower.saturating_mul(2)) / 3
}

#[derive(Clone, Debug)]
struct Slot {
	name: String,
	max: u64,
	stall: u64,
	priority: u8,
	demand: usize,
	unsupported: bool,
	requested: u64,
	applied: u64,
	stalled: bool,
	rate: Rate,
}

impl Slot {
	fn demanded(&self) -> bool {
		self.demand > 0
	}

	fn rate_policy(max: u64, stall: u64) -> Policy {
		let mut policy = Policy::new(max);
		// Subdivision already happened. Don't take a second headroom cut, and
		// hold at the band rather than the default max/10 floor.
		policy.headroom = 1.0;
		policy.min = stall.min(max);
		policy
	}
}

/// Ladder-wide rate and stall state. Crate-private; tests drive it directly.
#[derive(Clone, Debug)]
pub(crate) struct Control {
	slots: Vec<Slot>,
	estimate: Option<u64>,
	adaptive: bool,
}

impl Control {
	fn new<'a>(rungs: impl IntoIterator<Item = &'a Resolved>, adaptive: bool) -> Self {
		let mut slots: Vec<Slot> = rungs
			.into_iter()
			.map(|rung| Slot {
				name: rung.name.clone(),
				max: rung.bitrate,
				stall: 0,
				priority: PRIORITY.video,
				demand: 0,
				unsupported: false,
				requested: rung.bitrate,
				applied: rung.bitrate,
				stalled: false,
				rate: Rate::new(Slot::rate_policy(rung.bitrate, 0)),
			})
			.collect();
		slots.sort_by_key(|slot| slot.max);
		let n = slots.len();
		for i in 0..n {
			let lower = if i == 0 { 0 } else { slots[i - 1].max };
			let stall = stall_boundary(slots[i].max, lower);
			slots[i].stall = stall;
			slots[i].priority = PRIORITY.video.saturating_add((n - 1 - i) as u8);
			slots[i].rate = Rate::new(Slot::rate_policy(slots[i].max, stall));
		}
		Self {
			slots,
			estimate: None,
			adaptive,
		}
	}

	fn slot(&self, name: &str) -> Option<&Slot> {
		self.slots.iter().find(|slot| slot.name == name)
	}

	fn slot_mut(&mut self, name: &str) -> Option<&mut Slot> {
		self.slots.iter_mut().find(|slot| slot.name == name)
	}

	fn requested(&self, name: &str) -> Option<u64> {
		self.slot(name).map(|slot| slot.requested)
	}

	#[cfg(test)]
	fn applied(&self, name: &str) -> Option<u64> {
		self.slot(name).map(|slot| slot.applied)
	}

	fn priority(&self, name: &str) -> u8 {
		self.slot(name).map(|slot| slot.priority).unwrap_or(PRIORITY.video)
	}

	fn catalog_stalled(&self, name: &str) -> Option<bool> {
		self.slot(name).and_then(|slot| slot.stalled.then_some(true))
	}

	fn shares(&self) -> Vec<u64> {
		let mut remaining = self.estimate.unwrap_or(0);
		let mut shares = vec![0; self.slots.len()];
		for (i, slot) in self.slots.iter().enumerate() {
			if !slot.demanded() {
				continue;
			}
			let share = remaining.min(slot.max);
			shares[i] = share;
			remaining = remaining.saturating_sub(share);
		}
		shares
	}

	fn refresh_stalled(&mut self) {
		if !self.adaptive {
			for slot in &mut self.slots {
				slot.stalled = false;
			}
			return;
		}
		for slot in &mut self.slots {
			if slot.unsupported {
				continue;
			}
			// Catalog follows the last accepted target. At the boundary the
			// encoder has clamped, so equal-to-stall is stalled.
			slot.stalled = slot.applied <= slot.stall;
		}
	}

	fn recover_idle(&mut self) {
		if !self.adaptive {
			return;
		}
		let Some(estimate) = self.estimate else {
			return;
		};
		let mut remaining = estimate;
		for slot in &mut self.slots {
			if slot.demanded() {
				remaining = remaining.saturating_sub(remaining.min(slot.max));
				continue;
			}
			if !slot.stalled {
				continue;
			}
			// Hypothetical only: do not consume remaining or encode probe traffic.
			let share = remaining.min(slot.max);
			if slot.unsupported {
				if share >= slot.max {
					slot.stalled = false;
				}
			} else if share > slot.stall {
				slot.stalled = false;
			}
		}
	}

	fn step(&mut self, now: Instant) {
		if !self.adaptive {
			for slot in &mut self.slots {
				slot.requested = slot.max;
				slot.stalled = false;
			}
			return;
		}

		if self.estimate.is_none() {
			self.recover_idle();
			return;
		}

		let shares = self.shares();
		for (i, slot) in self.slots.iter_mut().enumerate() {
			if slot.unsupported {
				slot.requested = slot.max;
				slot.stalled = shares[i] < slot.max;
				continue;
			}
			if let Some(target) = slot.rate.update(Some(shares[i]), now) {
				slot.requested = target;
			} else {
				slot.requested = slot.rate.target();
			}
		}
		self.refresh_stalled();
		self.recover_idle();
	}

	fn set_estimate(&mut self, estimate: Option<u64>, now: Instant) {
		self.estimate = estimate;
		self.step(now);
	}

	fn add_demand(&mut self, name: &str, now: Instant) {
		if let Some(slot) = self.slot_mut(name) {
			slot.demand = slot.demand.saturating_add(1);
		}
		self.step(now);
	}

	fn remove_demand(&mut self, name: &str, now: Instant) {
		if let Some(slot) = self.slot_mut(name) {
			slot.demand = slot.demand.saturating_sub(1);
		}
		self.step(now);
	}

	fn accept(&mut self, name: &str, bitrate: u64) {
		let adaptive = self.adaptive;
		{
			let Some(slot) = self.slot_mut(name) else {
				return;
			};
			slot.applied = bitrate;
			if slot.unsupported {
				return;
			}
			if !adaptive {
				slot.stalled = false;
				return;
			}
			slot.stalled = slot.applied <= slot.stall;
		}
		self.recover_idle();
	}

	fn unsupported(&mut self, name: &str, now: Instant) {
		if let Some(slot) = self.slot_mut(name) {
			slot.unsupported = true;
			slot.applied = slot.max;
			slot.requested = slot.max;
			tracing::warn!(
				rung = %name,
				max = slot.max,
				"encoder cannot follow the ladder target; holding the configured maximum"
			);
		}
		self.step(now);
	}
}

/// Shared handle the transcoder and every rung clone.
#[derive(Clone)]
pub(crate) struct Producer {
	state: kio::Producer<State>,
}

struct State {
	control: Control,
	rev: u64,
}

impl Producer {
	pub(crate) fn new<'a>(rungs: impl IntoIterator<Item = &'a Resolved>, adaptive: bool) -> Self {
		Self {
			state: kio::Producer::new(State {
				control: Control::new(rungs, adaptive),
				rev: 0,
			}),
		}
	}

	fn modify(&self, f: impl FnOnce(&mut Control)) {
		let Ok(mut state) = self.state.write() else {
			return;
		};
		let before = snapshot(&state.control);
		f(&mut state.control);
		if snapshot(&state.control) != before {
			state.rev = state.rev.wrapping_add(1);
		}
	}

	pub(crate) fn set_estimate(&self, estimate: Option<u64>, now: Instant) {
		self.modify(|control| control.set_estimate(estimate, now));
	}

	pub(crate) fn add_demand(&self, name: &str, now: Instant) {
		self.modify(|control| control.add_demand(name, now));
	}

	pub(crate) fn remove_demand(&self, name: &str, now: Instant) {
		self.modify(|control| control.remove_demand(name, now));
	}

	pub(crate) fn accept(&self, name: &str, bitrate: u64) {
		self.modify(|control| control.accept(name, bitrate));
	}

	pub(crate) fn unsupported(&self, name: &str, now: Instant) {
		self.modify(|control| control.unsupported(name, now));
	}

	pub(crate) fn reconcile<'a>(&self, rungs: impl IntoIterator<Item = &'a Resolved>, now: Instant) {
		let rungs: Vec<&Resolved> = rungs.into_iter().collect();
		self.modify(|control| {
			let estimate = control.estimate;
			let adaptive = control.adaptive;
			let old: HashMap<String, Slot> = control
				.slots
				.iter()
				.map(|slot| (slot.name.clone(), slot.clone()))
				.collect();
			*control = Control::new(rungs, adaptive);
			control.estimate = estimate;
			for slot in &mut control.slots {
				let Some(prev) = old.get(&slot.name) else {
					continue;
				};
				slot.demand = prev.demand;
				slot.unsupported = prev.unsupported;
				slot.requested = prev.requested;
				slot.applied = prev.applied;
				slot.stalled = prev.stalled;
				slot.rate = prev.rate.clone();
			}
			control.step(now);
		});
	}

	pub(crate) fn requested(&self, name: &str) -> Option<u64> {
		self.state.read().control.requested(name)
	}

	pub(crate) fn priority(&self, name: &str) -> u8 {
		self.state.read().control.priority(name)
	}

	pub(crate) fn catalog_stalled(&self, name: &str) -> Option<bool> {
		self.state.read().control.catalog_stalled(name)
	}

	pub(crate) fn consume(&self) -> Consumer {
		Consumer {
			state: self.state.consume(),
			last_rev: self.state.read().rev,
		}
	}
}

fn snapshot(control: &Control) -> Vec<(u64, u64, bool, usize, bool)> {
	control
		.slots
		.iter()
		.map(|slot| {
			(
				slot.requested,
				slot.applied,
				slot.stalled,
				slot.demand,
				slot.unsupported,
			)
		})
		.collect()
}

/// Cursor that wakes when requested targets or catalog stall bits move.
pub(crate) struct Consumer {
	state: kio::Consumer<State>,
	last_rev: u64,
}

impl Consumer {
	/// Wait until the controller publishes a new revision.
	///
	/// Returns `false` once the producer is gone.
	pub(crate) async fn changed(&mut self) -> bool {
		let last = self.last_rev;
		match self
			.state
			.wait(|state| {
				if state.rev != last {
					Poll::Ready(state.rev)
				} else {
					Poll::Pending
				}
			})
			.await
		{
			Ok(rev) => {
				self.last_rev = rev;
				true
			}
			Err(_) => false,
		}
	}
}

/// RAII demand: a live session or in-flight fetch holds one until drop.
pub(crate) struct Demand {
	control: Producer,
	name: String,
}

impl Demand {
	pub(crate) fn new(control: Producer, name: impl Into<String>, now: Instant) -> Self {
		let name = name.into();
		control.add_demand(&name, now);
		Self { control, name }
	}
}

impl Drop for Demand {
	fn drop(&mut self) {
		self.control.remove_demand(&self.name, Instant::now());
	}
}

/// Copy each rung's catalog `stalled` bit from the last applied target.
pub(crate) fn apply_stalled(rungs: &mut [Published], control: &Producer) {
	for published in rungs {
		published.entry.stalled = control.catalog_stalled(&published.rung.name);
	}
}

#[cfg(test)]
mod tests {
	use std::time::Duration;

	use super::*;

	fn resolved(height: u32, bitrate: u64) -> Resolved {
		Resolved {
			name: format!("video/{height}p"),
			height,
			size: moq_video::Size::new((height * 16 / 9) & !1, height),
			bitrate,
			framerate: 30,
		}
	}

	fn adaptive(rungs: &[(u32, u64)]) -> Control {
		let rungs: Vec<Resolved> = rungs.iter().map(|(h, br)| resolved(*h, *br)).collect();
		Control::new(rungs.iter(), true)
	}

	fn fixed(rungs: &[(u32, u64)]) -> Control {
		let rungs: Vec<Resolved> = rungs.iter().map(|(h, br)| resolved(*h, *br)).collect();
		Control::new(rungs.iter(), false)
	}

	fn accept_requested(control: &mut Control, name: &str) {
		let requested = control.requested(name).unwrap();
		control.accept(name, requested);
	}

	#[test]
	fn five_megabit_over_two_and_a_half_lands_near_3_33() {
		assert_eq!(stall_boundary(5_000_000, 2_500_000), 3_333_333);
	}

	#[test]
	fn default_lowest_rung_stalls_near_117k() {
		assert_eq!(stall_boundary(350_000, 0), 116_666);
	}

	#[test]
	fn one_demanded_rung_reaches_max_on_a_permissive_uplink() {
		let mut control = adaptive(&[(1080, 5_000_000)]);
		let now = Instant::now();
		control.add_demand("video/1080p", now);
		control.set_estimate(Some(20_000_000), now);
		accept_requested(&mut control, "video/1080p");
		assert_eq!(control.applied("video/1080p"), Some(5_000_000));
		assert_eq!(control.catalog_stalled("video/1080p"), None);
	}

	#[test]
	fn lower_rungs_are_protected_when_several_share_one_uplink() {
		let mut control = adaptive(&[(240, 350_000), (720, 2_500_000), (1080, 5_000_000)]);
		let now = Instant::now();
		control.add_demand("video/240p", now);
		control.add_demand("video/720p", now);
		control.add_demand("video/1080p", now);
		// 3 Mbps: the 240p ceiling fits, 720p takes the rest, 1080p gets nothing.
		control.set_estimate(Some(3_000_000), now);
		accept_requested(&mut control, "video/240p");
		accept_requested(&mut control, "video/720p");
		accept_requested(&mut control, "video/1080p");

		assert_eq!(control.applied("video/240p"), Some(350_000));
		assert_eq!(control.catalog_stalled("video/240p"), None);
		assert_eq!(control.applied("video/720p"), Some(2_500_000));
		assert_eq!(control.catalog_stalled("video/720p"), None);
		assert_eq!(
			control.applied("video/1080p"),
			Some(stall_boundary(5_000_000, 2_500_000))
		);
		assert_eq!(control.catalog_stalled("video/1080p"), Some(true));
	}

	#[test]
	fn advertised_maximum_does_not_follow_the_applied_target() {
		let rungs = [resolved(1080, 5_000_000), resolved(720, 2_500_000)];
		let mut control = Control::new(rungs.iter(), true);
		let now = Instant::now();
		control.add_demand("video/1080p", now);
		control.set_estimate(Some(1_000_000), now);
		accept_requested(&mut control, "video/1080p");
		assert_eq!(control.slot("video/1080p").unwrap().max, 5_000_000);
		assert_eq!(
			control.applied("video/1080p"),
			Some(stall_boundary(5_000_000, 2_500_000))
		);
		assert_eq!(control.catalog_stalled("video/1080p"), Some(true));
	}

	#[test]
	fn supported_rung_adapts_clamps_stalls_and_recovers() {
		let mut control = adaptive(&[(720, 2_500_000), (1080, 5_000_000)]);
		let start = Instant::now();
		control.add_demand("video/1080p", start);
		control.set_estimate(Some(20_000_000), start);
		accept_requested(&mut control, "video/1080p");
		assert_eq!(control.applied("video/1080p"), Some(5_000_000));
		assert_eq!(control.catalog_stalled("video/1080p"), None);

		control.set_estimate(Some(1_000_000), start + Duration::from_millis(10));
		accept_requested(&mut control, "video/1080p");
		let stall = stall_boundary(5_000_000, 2_500_000);
		assert_eq!(control.applied("video/1080p"), Some(stall));
		assert_eq!(control.catalog_stalled("video/1080p"), Some(true));
		assert_eq!(control.slot("video/1080p").unwrap().max, 5_000_000);

		// Ramp has plenty of room: a minute at 25%/s walks stall back to max.
		control.set_estimate(Some(20_000_000), start + Duration::from_secs(60));
		accept_requested(&mut control, "video/1080p");
		assert_eq!(control.applied("video/1080p"), Some(5_000_000));
		assert_eq!(control.catalog_stalled("video/1080p"), None);
	}

	#[test]
	fn catalog_follows_applied_not_requested() {
		let mut control = adaptive(&[(1080, 5_000_000)]);
		let now = Instant::now();
		control.add_demand("video/1080p", now);
		control.set_estimate(Some(1_000), now);
		// Request has clamped to the band, but nothing has been accepted yet.
		assert_eq!(control.requested("video/1080p"), Some(stall_boundary(5_000_000, 0)));
		assert_eq!(control.applied("video/1080p"), Some(5_000_000));
		assert_eq!(control.catalog_stalled("video/1080p"), None);

		accept_requested(&mut control, "video/1080p");
		assert_eq!(control.catalog_stalled("video/1080p"), Some(true));
	}

	#[test]
	fn transient_failure_keeps_the_last_applied_target() {
		let mut control = adaptive(&[(1080, 5_000_000)]);
		let now = Instant::now();
		control.add_demand("video/1080p", now);
		control.set_estimate(Some(20_000_000), now);
		accept_requested(&mut control, "video/1080p");
		control.set_estimate(Some(1_000), now + Duration::from_millis(10));
		// Encoder rejected the clamp. Applied stays at the last success.
		assert_eq!(control.requested("video/1080p"), Some(stall_boundary(5_000_000, 0)));
		assert_eq!(control.applied("video/1080p"), Some(5_000_000));
		assert_eq!(control.catalog_stalled("video/1080p"), None);
	}

	#[test]
	fn unsupported_encoder_holds_max_and_does_not_recover_early() {
		let mut control = adaptive(&[(720, 2_500_000), (1080, 5_000_000)]);
		let now = Instant::now();
		control.add_demand("video/1080p", now);
		control.set_estimate(Some(4_000_000), now);
		control.unsupported("video/1080p", now);
		assert_eq!(control.applied("video/1080p"), Some(5_000_000));
		assert_eq!(control.catalog_stalled("video/1080p"), Some(true));

		// Allocation is still below the configured maximum, so it stays stalled.
		control.set_estimate(Some(4_500_000), now + Duration::from_secs(60));
		assert_eq!(control.applied("video/1080p"), Some(5_000_000));
		assert_eq!(control.catalog_stalled("video/1080p"), Some(true));

		control.set_estimate(Some(8_000_000), now + Duration::from_secs(61));
		assert_eq!(control.catalog_stalled("video/1080p"), None);
	}

	#[test]
	fn idle_stalled_rung_recovers_from_a_hypothetical_share() {
		let mut control = adaptive(&[(240, 350_000), (1080, 5_000_000)]);
		let now = Instant::now();
		control.add_demand("video/1080p", now);
		control.set_estimate(Some(50_000), now);
		accept_requested(&mut control, "video/1080p");
		assert_eq!(control.catalog_stalled("video/1080p"), Some(true));

		control.remove_demand("video/1080p", now);
		// Still stalled: the hypothetical share is the full 50 kbps, under the band.
		assert_eq!(control.catalog_stalled("video/1080p"), Some(true));

		control.set_estimate(Some(4_000_000), now + Duration::from_millis(1));
		assert_eq!(control.catalog_stalled("video/1080p"), None);
	}

	#[test]
	fn no_bandwidth_input_never_publishes_stalled() {
		let mut control = fixed(&[(240, 350_000), (1080, 5_000_000)]);
		let now = Instant::now();
		control.add_demand("video/1080p", now);
		control.set_estimate(Some(1), now);
		accept_requested(&mut control, "video/1080p");
		assert_eq!(control.applied("video/1080p"), Some(5_000_000));
		assert_eq!(control.requested("video/1080p"), Some(5_000_000));
		assert_eq!(control.catalog_stalled("video/1080p"), None);
		assert_eq!(control.catalog_stalled("video/240p"), None);
	}

	#[test]
	fn priorities_descend_the_ladder() {
		let control = adaptive(&[(240, 350_000), (360, 600_000), (720, 2_500_000)]);
		assert!(control.priority("video/240p") > control.priority("video/360p"));
		assert!(control.priority("video/360p") > control.priority("video/720p"));
		assert_eq!(control.priority("video/720p"), PRIORITY.video);
	}

	#[test]
	fn apply_stalled_writes_catalog_bits_from_applied() {
		let rungs = [resolved(1080, 5_000_000)];
		let producer = Producer::new(rungs.iter(), true);
		let now = Instant::now();
		producer.add_demand("video/1080p", now);
		producer.set_estimate(Some(1_000), now);
		producer.accept("video/1080p", producer.requested("video/1080p").unwrap());

		let mut published = [Published {
			rung: rungs[0].clone(),
			entry: hang::catalog::VideoConfig::new(hang::catalog::H264 {
				inline: true,
				profile: 0x42,
				constraints: 0,
				level: 30,
			}),
		}];
		apply_stalled(&mut published, &producer);
		assert_eq!(published[0].entry.stalled, Some(true));
	}
}
