//! Archive format versions and the version window this reader accepts.

use crate::FormatError;

/// Archive format version as stored in the header (`major << 24 | minor << 16 | patch`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FormatVersion {
    pub major: u8,
    pub minor: u8,
    pub patch: u16,
}

impl FormatVersion {
    /// Factory function.
    ///
    /// # Returns
    ///
    /// The version encoded in `packed` (`major << 24 | minor << 16 | patch`).
    #[must_use]
    pub const fn from_packed(packed: u32) -> Self {
        Self {
            major: packed.to_be_bytes()[0],
            minor: packed.to_be_bytes()[1],
            patch: u16::from_be_bytes([packed.to_be_bytes()[2], packed.to_be_bytes()[3]]),
        }
    }

    /// # Returns
    ///
    /// The packed representation (`major << 24 | minor << 16 | patch`).
    #[must_use]
    pub const fn to_packed(self) -> u32 {
        let patch = self.patch.to_be_bytes();
        u32::from_be_bytes([self.major, self.minor, patch[0], patch[1]])
    }
}

impl std::fmt::Display for FormatVersion {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// The decoder family an archive is read with; the only output of version checks.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FormatEpoch {
    /// 0.5.x: Timestamp columns, `\` patterns, epoch-millisecond timestamp ranges.
    V0_5,
}

/// A version classified against what this reader knows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ClassifiedVersion {
    pub version: FormatVersion,
    pub epoch: FormatEpoch,

    /// Whether the archive comes from a newer 0.5 patch release than this reader knows; unknown
    /// constructs in such archives fail as [`FormatError::UnsupportedFeature`].
    pub is_newer_patch: bool,
}

impl ClassifiedVersion {
    /// The newest 0.5 patch release this reader knows.
    pub const KNOWN_PATCH: u16 = 1;

    /// Classifies `version`; only [0.5.0, 0.5.x] is accepted in this round.
    ///
    /// # Returns
    ///
    /// The classified version on success.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    ///
    /// * [`FormatError::UnsupportedVersion`] if `version` is outside [0.5.0, 0.5.x].
    pub const fn classify(version: FormatVersion) -> Result<Self, FormatError> {
        if 0 != version.major || 5 != version.minor {
            return Err(FormatError::UnsupportedVersion(version));
        }
        Ok(Self {
            version,
            epoch: FormatEpoch::V0_5,
            is_newer_patch: version.patch > Self::KNOWN_PATCH,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::ClassifiedVersion;
    use super::FormatEpoch;
    use super::FormatVersion;
    use crate::FormatError;

    #[test]
    fn packed_version_round_trips() {
        const PACKED_0_5_1: u32 = 0x0005_0001;

        let version = FormatVersion::from_packed(PACKED_0_5_1);
        assert_eq!(
            version,
            FormatVersion {
                major: 0,
                minor: 5,
                patch: 1,
            }
        );
        assert_eq!(version.to_packed(), PACKED_0_5_1);

        let largest = FormatVersion {
            major: u8::MAX,
            minor: u8::MAX,
            patch: u16::MAX,
        };
        assert_eq!(largest.to_packed(), u32::MAX);
        assert_eq!(FormatVersion::from_packed(u32::MAX), largest);
    }

    #[test]
    fn version_displays_as_dotted_triple() {
        assert_eq!(
            FormatVersion::from_packed(0x0005_0203).to_string(),
            "0.5.515"
        );
    }

    #[test]
    fn only_the_0_5_window_is_accepted() -> anyhow::Result<()> {
        for (packed, is_newer_patch) in [
            (0x0005_0000, false),
            (0x0005_0001, false),
            (0x0005_0002, true),
        ] {
            let classified = ClassifiedVersion::classify(FormatVersion::from_packed(packed))?;
            assert_eq!(classified.epoch, FormatEpoch::V0_5);
            assert_eq!(classified.version.to_packed(), packed);
            assert_eq!(
                classified.is_newer_patch, is_newer_patch,
                "version {packed:#x}"
            );
        }

        for packed in [0x0004_0000, 0x0004_ffff, 0x0006_0000, 0x0105_0001, 0] {
            let version = FormatVersion::from_packed(packed);
            let error = ClassifiedVersion::classify(version)
                .expect_err("a version outside [0.5.0, 0.5.x] must be rejected");
            assert!(
                matches!(error, FormatError::UnsupportedVersion(rejected) if rejected == version),
                "{error:?}"
            );
        }
        Ok(())
    }
}
