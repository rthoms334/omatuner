-- Tuner window rules for Omarchy 4 (Hyprland Lua config).
-- Install: append to ~/.config/hypr/hyprland.lua BELOW `require("default.hypr.toggles")`,
-- or `require("tuner")` after copying this file to ~/.config/hypr/tuner.lua.

-- Video must be fully opaque: opt out of Omarchy's default 0.985/0.96 opacity (like mpv/vlc).
o.window("^(tuner)$", { tag = "-default-opacity" })
o.window("^(tuner)$", { opacity = "1 1" })

-- Keep sending frame callbacks while Tuner is on another workspace / hidden (at
-- misc:render_unfocused_fps, default 15). Without this Hyprland stops calling the app, mpv's
-- video output stalls and Tuner falls back to suspending video decoding (audio continues).
o.window("^(tuner)$", { render_unfocused = true })

-- Tuner inhibits idle itself (Wayland zwp_idle_inhibit while video plays, honoured by the
-- Omarchy idle service). If your setup ignores client inhibitors, uncomment the fallback:
-- o.window("^(tuner)$", { idle_inhibit = "fullscreen" })

-- Optional launcher binding (Super+Shift+T is free in Omarchy 4 defaults):
-- o.bind("SUPER + SHIFT + T", "Tuner", o.launch_sole("tuner", "tuner"))
