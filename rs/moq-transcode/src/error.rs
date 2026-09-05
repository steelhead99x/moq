//! Error type for the transcoder.

/// Errors returned by `moq-transcode`.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
	/// The source catalog has no rendition the transcoder can decode: it needs
	/// an H.264, H.265, or AV1 rendition local to the source broadcast.
	#[error("no transcodable video rendition in the source catalog")]
	NoSource,

	/// The chosen source rendition doesn't declare coded dimensions, so rungs
	/// can't be sized or gated against it.
	#[error("source rendition {0:?} is missing codedWidth/codedHeight")]
	SourceDimensions(String),

	/// Two configured rungs share the same maximum bitrate, so the ladder has no
	/// lower-is-lower reading.
	#[error("ladder rungs at {height_a}px and {height_b}px share the bitrate ceiling {bitrate}")]
	DuplicateCeiling {
		/// Height of one of the colliding rungs, in pixels.
		height_a: u32,
		/// Height of the other colliding rung, in pixels.
		height_b: u32,
		/// The shared bitrate ceiling, in bits per second.
		bitrate: u64,
	},

	/// Coded height decreases as configured bitrate increases.
	#[error("ladder inversion: {tall}px@{cheap} is taller than {short}px@{expensive} but cheaper")]
	ResolutionInversion {
		/// The taller rung's height, in pixels.
		tall: u32,
		/// The taller rung's bitrate, in bits per second.
		cheap: u64,
		/// The shorter rung's height, in pixels.
		short: u32,
		/// The shorter rung's bitrate, in bits per second.
		expensive: u64,
	},

	/// moq-net transport error.
	#[error(transparent)]
	Net(#[from] moq_net::Error),

	/// moq-mux container/catalog error.
	#[error(transparent)]
	Mux(#[from] moq_mux::Error),

	/// hang catalog/container error.
	#[error(transparent)]
	Hang(#[from] hang::Error),

	/// Video decode/encode error.
	#[error(transparent)]
	Video(#[from] moq_video::Error),

	/// Timestamp overflow converting to the moq microsecond timescale.
	#[error(transparent)]
	TimeOverflow(#[from] moq_net::TimeOverflow),

	/// Frame scaling failure.
	#[error("scale failed: {0}")]
	Scale(String),
}
