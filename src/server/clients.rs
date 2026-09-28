//! client registry, writer tasks, render scheduler, attach/detach.
//!
//! T2.0 lands the registry entry as a stub; T2.5a fills it.

/// One attached client, keyed by `ClientId` in `State::clients`. T2.5a adds
/// its fields (session, size, caps, writer, last Screen, activity).
#[derive(Debug, Default)]
pub struct ClientEntry {}
