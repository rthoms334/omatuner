-- Tuner window rules for Omarchy 4 (Hyprland Lua config).
-- Install: copy this file to ~/.config/hypr/tuner.lua and add `require("hypr.tuner")` to
-- ~/.config/hypr/hyprland.lua BELOW `require("default.hypr.toggles")` (or paste its lines
-- there). The rules must come after Omarchy's defaults, which tag every window for opacity.
-- Window class = Tuner's Wayland app id: `tuner`, or `tuner-popout` when Tuner starts straight
-- into the pop-out (a scheduled switch while Tuner is closed, `tuner --play <ref> --popout`).

-- Video must be fully opaque: opt out of Omarchy's default 0.985/0.96 opacity (like mpv/vlc).
o.window("^(tuner|tuner-popout)$", { tag = "-default-opacity" })
o.window("^(tuner|tuner-popout)$", { opacity = "1 1" })

-- Keep sending frame callbacks while Tuner is on another workspace / hidden (at
-- misc:render_unfocused_fps, default 15). Without this Hyprland stops calling the app, mpv's
-- video output stalls and Tuner falls back to suspending video decoding (audio continues).
o.window("^(tuner|tuner-popout)$", { render_unfocused = true })

-- A Tuner started for the pop-out appears floating and pinned on the workspace you are on and
-- never takes the keyboard from what you are typing in (the webcam-overlay pattern). Tuner then
-- sizes, places and tags it like any pop-out; popping it back in tiles it. Only that window
-- gets this class: a normal launch is `tuner` and is focused as usual.
o.window("^(tuner-popout)$", { float = true, pin = true, no_initial_focus = true })

-- Tuner inhibits idle itself (Wayland zwp_idle_inhibit while video plays, honoured by the
-- Omarchy idle service). If your setup ignores client inhibitors, uncomment the fallback:
-- o.window("^(tuner|tuner-popout)$", { idle_inhibit = "fullscreen" })

-- Optional launcher binding (Super+Shift+T is free in Omarchy 4 defaults):
-- o.bind("SUPER + SHIFT + T", "Tuner", o.launch_sole("^tuner(-popout)?$", "tuner"))
