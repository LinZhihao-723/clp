//! Format tests over the C++-written fixture corpus: structure counts against an independent
//! reading of every archive, crafted archives for the registered format divergences and scope
//! exclusions, and mutation tests that must fail with typed errors and never panic.

mod crafted;
mod fixtures;
mod mutations;
mod support;
