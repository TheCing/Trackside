//! friendlyplugins — Heaven's NATIVE, in-process stand-ins for the popular companion plugins, so
//! you get their functionality WITHOUT loading their DLLs (or running Hachimi at all):
//!
//!   - **horseACT** (ayaliz/horseACT) — runtime race + trained-uma data dump in the Hakuraku-
//!     compatible format. Heaven does this natively: `race_export` (races — Career / Room match /
//!     Champions meeting / Practice room), `htt` (Team Trials results) and `umas` (veteran roster),
//!     all byte-compatible with horseACT v1.1.4. Works after the game updates that broke the plugin.
//!   - **CarrotBlender** (qwcan/CarrotBlender) — feeds the decrypted game responses to companion
//!     overlays (e.g. UmaOverlay-lite). Heaven does this natively in `uma_bridge` (UDP 17229, our own
//!     AES key/iv, driven by the update-proof DecompressResponse hook — no external plugin needed).
//!
//! This module is a thin coordinator: it owns the on/off for the CarrotBlender-style companion feed
//! and groups everything under one menu section. The heavy lifting stays in each feature's own
//! module (per-feature separation preserved). Server upload (horseACT's apiKey/serverUrl) is a
//! separate, protocol-specific follow-up — Heaven's local Hakuraku files already cover the exports.

#![allow(dead_code)]

/// The CarrotBlender-equivalent companion feed (game responses → companion overlays over UDP).
/// Default ON: it's passive (only does anything when an overlay actually connects).
pub fn bridge_enabled() -> bool {
    crate::uma_bridge::is_enabled()
}
pub fn set_bridge_enabled(on: bool) {
    crate::uma_bridge::set_enabled(on);
}

/// Apply the persisted companion-feed state at boot.
pub fn apply(s: &crate::settings::Settings) {
    crate::uma_bridge::set_enabled(s.companion_bridge);
}
