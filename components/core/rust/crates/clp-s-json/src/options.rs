//! Rendering options.

/// Rendering choices that change output bytes; the defaults are the C++ output plus the
/// registered fixes (DV-01, DV-02).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct JsonOptions {
    /// Print plain floats like the C++ `%f`-based writer instead of shortest round-trip (for the
    /// differential harness only).
    pub should_emulate_cpp_plain_floats: bool,
}
