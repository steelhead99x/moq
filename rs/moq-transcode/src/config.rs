//! Transcoder configuration: the rung ladder and catalog wiring.

use moq_net::{AsPath, PathRelativeOwned};

use crate::Error;

#[doc(hidden)]
#[deprecated(note = "use moq_net::Path::relative")]
pub fn source_reference(source: impl AsPath, output: impl AsPath) -> Option<PathRelativeOwned> {
	let source = source.as_path();
	let output = output.as_path();
	if output.strip_prefix(&source)?.is_empty() {
		return None;
	}

	source.relative(&output)
}

/// One candidate output rendition: a target resolution (by height) and bitrate.
///
/// The width is derived from the source aspect ratio at runtime, and a rung is
/// only offered when it is strictly below the source (see [`Config::rungs`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Rung {
	/// Output height in pixels. Rounded down to even (I420 chroma is 2x2).
	pub height: u32,

	/// Target bitrate in bits per second: the CBR target and the bitrate
	/// advertised in the derivative catalog.
	pub bitrate: u64,
}

impl Rung {
	/// A rung at `height` pixels and `bitrate` bits per second.
	pub fn new(height: u32, bitrate: u64) -> Self {
		Self { height, bitrate }
	}
}

/// Transcoder configuration for [`run`](crate::run).
///
/// `#[non_exhaustive]`: build via `Config::default()` and set fields, so future
/// knobs don't break callers.
#[derive(Clone)]
#[non_exhaustive]
pub struct Config {
	/// Candidate output renditions. Only rungs strictly below the source
	/// survive: a rung is dropped when its height exceeds the source, when its
	/// bitrate is not below the source bitrate (when known), or when it matches
	/// the source height without a known source bitrate to undercut. A 480p
	/// source is never transcoded up to 720p.
	pub rungs: Vec<Rung>,

	/// Where the source broadcast lives relative to the output broadcast, e.g.
	/// `"."` when the output is published at `<source>/transcode.hang`. When
	/// set, the derivative catalog references the source renditions (all video
	/// and audio) through this path so players fetch them from the source
	/// directly; the transcoder never proxies or subscribes them. `None` omits
	/// them from the derivative catalog.
	pub source: Option<PathRelativeOwned>,

	/// Which video encoder implementation encodes the rungs. The default
	/// prefers hardware (NVENC on Linux, VideoToolbox on macOS, Media
	/// Foundation on Windows) and falls back to openh264.
	pub encoder: moq_video::encode::Kind,

	/// Which video decoder implementation decodes the source. The default
	/// prefers hardware and falls back to openh264 (H.264 only; H.265 sources
	/// need a hardware decoder).
	pub decoder: moq_video::decode::Kind,

	/// Frame resize behavior. Automatic mode keeps GPU-backed frames on the GPU.
	pub resize: moq_video::resize::Config,

	/// Optional uplink send-bandwidth estimate the ladder controller follows.
	///
	/// `None` keeps today's fixed-rate behavior: every demanded rung encodes at
	/// its configured maximum and the catalog never publishes congestion-induced
	/// `stalled` state. Supply a [`moq_net::bandwidth::Consumer`] (the CLI wires
	/// the publisher session's send estimate) to subdivide that estimate across
	/// the ladder and advertise `stalled` from the last target each encoder
	/// accepted.
	pub bandwidth: Option<moq_net::bandwidth::Consumer>,
}

impl std::fmt::Debug for Config {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Config")
			.field("rungs", &self.rungs)
			.field("source", &self.source)
			.field("encoder", &self.encoder)
			.field("decoder", &self.decoder)
			.field("resize", &self.resize)
			.field("bandwidth", &self.bandwidth.as_ref().map(|_| "Some(..)"))
			.finish()
	}
}

impl Default for Config {
	fn default() -> Self {
		Self {
			// The default ladder, top rung first, filtered against the source at
			// runtime so only strictly-lower renditions are offered.
			rungs: vec![
				Rung::new(1080, 5_000_000),
				Rung::new(720, 2_500_000),
				Rung::new(480, 1_200_000),
				Rung::new(360, 600_000),
				Rung::new(240, 350_000),
			],
			source: None,
			encoder: moq_video::encode::Kind::default(),
			decoder: moq_video::decode::Kind::default(),
			resize: moq_video::resize::Config::default(),
			bandwidth: None,
		}
	}
}

/// Order configured rungs into strictly ascending maximum bitrate.
///
/// Rejects two shapes rather than guessing an order: duplicate ceilings, and
/// configurations whose coded height decreases as bitrate increases when both
/// heights are known. A height of 0 means the picture is not known yet, so that
/// inversion check is skipped for that rung.
pub fn order_rungs(rungs: &[Rung]) -> Result<Vec<Rung>, Error> {
	for (i, a) in rungs.iter().enumerate() {
		for b in &rungs[i + 1..] {
			if a.bitrate == b.bitrate {
				return Err(Error::DuplicateCeiling {
					height_a: a.height,
					height_b: b.height,
					bitrate: a.bitrate,
				});
			}
			if a.height == 0 || b.height == 0 {
				continue;
			}
			let (cheaper, dearer) = if a.bitrate < b.bitrate { (a, b) } else { (b, a) };
			if cheaper.height > dearer.height {
				return Err(Error::ResolutionInversion {
					tall: cheaper.height,
					cheap: cheaper.bitrate,
					short: dearer.height,
					expensive: dearer.bitrate,
				});
			}
		}
	}

	let mut ordered = rungs.to_vec();
	ordered.sort_by_key(|rung| rung.bitrate);
	Ok(ordered)
}

impl Config {
	/// Configured rungs in strictly ascending bitrate order.
	///
	/// See [`order_rungs`] for the shapes that are refused.
	pub fn ordered_rungs(&self) -> Result<Vec<Rung>, Error> {
		order_rungs(&self.rungs)
	}
}

#[cfg(test)]
#[allow(deprecated)]
mod tests {
	use super::*;

	#[test]
	fn source_reference_normalizes_and_counts_output_depth() {
		assert_eq!(source_reference("a/b", "a/b/transcode.hang").unwrap().as_str(), ".");
		assert_eq!(source_reference("/a//b/", "a/b/dir/").unwrap().as_str(), ".");
		assert_eq!(
			source_reference("a/b", "a/b/dir/transcode.hang").unwrap().as_str(),
			".."
		);
		assert_eq!(
			source_reference("a/b", "a/b/one/two/transcode.hang").unwrap().as_str(),
			"../.."
		);
		assert!(source_reference("a/b", "other/transcode.hang").is_none());
		assert!(source_reference("a/b", "a/b").is_none());
	}

	#[test]
	fn orders_a_custom_ladder_given_out_of_order() {
		let config = Config {
			rungs: vec![
				Rung::new(720, 2_500_000),
				Rung::new(240, 350_000),
				Rung::new(1080, 5_000_000),
				Rung::new(360, 600_000),
			],
			..Default::default()
		};
		let ordered = config.ordered_rungs().unwrap();
		assert_eq!(
			ordered.iter().map(|r| (r.height, r.bitrate)).collect::<Vec<_>>(),
			[(240, 350_000), (360, 600_000), (720, 2_500_000), (1080, 5_000_000)]
		);
	}

	#[test]
	fn rejects_a_duplicate_ceiling() {
		let err = order_rungs(&[Rung::new(720, 2_500_000), Rung::new(480, 2_500_000)]).unwrap_err();
		assert!(matches!(err, Error::DuplicateCeiling { bitrate: 2_500_000, .. }));
	}

	#[test]
	fn rejects_a_resolution_bitrate_inversion() {
		let err = order_rungs(&[Rung::new(360, 2_500_000), Rung::new(720, 600_000)]).unwrap_err();
		assert!(matches!(
			err,
			Error::ResolutionInversion {
				tall: 720,
				cheap: 600_000,
				short: 360,
				expensive: 2_500_000,
			}
		));
	}

	#[test]
	fn unknown_dimensions_skip_the_inversion_check() {
		let ordered = order_rungs(&[Rung::new(0, 2_500_000), Rung::new(0, 600_000)]).unwrap();
		assert_eq!(
			ordered.iter().map(|r| r.bitrate).collect::<Vec<_>>(),
			[600_000, 2_500_000]
		);
	}
}
