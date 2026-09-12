//! Japanese input, as the two halves that have no screen and no keyboard.
//!
//! What is here is decidable by its tests: a table from romaji to kana, and a
//! sorted dictionary that answers what a reading could mean. Neither knows
//! where it is drawn, which pane asked, or what key was pressed to get here —
//! `tos-compositor` owns all of that, per `docs/design/ime.md`.

pub mod dict;
pub mod romaji;
