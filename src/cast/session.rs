//! One task per cast (DESIGN §40 §5.2, §8.5, §9.4): drives a `receiver::Backend` and a
//! `relay::RelaySession` from `player::remote::RemoteCmd`s, emits `RemoteEvent`s, escalates one
//! level (L2 → L3) once, runs the 10 s fetch watchdog (no playlist fetch after a 2xx insert →
//! `Firewall` when detection says Blocked/Unknown, else `ReceiverFailed`), and adds the item's
//! offset to every position. Also Test on TV. OWNER: CAST-6.

// CAST-6
