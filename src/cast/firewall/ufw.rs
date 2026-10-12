//! The `/etc/ufw/user.rules` parser (DESIGN §40 §9.4 "Detection"). OWNER: CAST-7.
//!
//! Reads the `### tuple ###` lines (format from ufw's own reader, `backend_iptables.py`
//! L697–790): action, proto, dport, dst, sport, src, `dir[_iface]`, the app fields when present,
//! then `comment=<hex>` (stripped first). Actions `allow|deny|reject|limit` with `_log` /
//! `_log-all`; `route:` rules ignored; port lists and ranges; proto `any`. Finds the covering
//! allow for the media port, an earlier shadowing deny (first match), and the rules tagged
//! `omatuner cast`. Pure: tested on fixture files (`tests/fixtures/cast/ufw/`).

// CAST-7
