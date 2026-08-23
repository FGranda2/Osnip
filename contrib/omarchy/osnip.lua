-- osnip — Omarchy / Hyprland integration.
--
-- This file is only the part a shell plugin cannot do for you: Hyprland
-- window rules and a keybinding. Capture, clipboard pinning, closing
-- pins, and per-pin actions all live in the Osnip bar plugin:
--
--     omarchy plugin add https://github.com/FGranda2/Osnip.git --enable
--
-- Install:
--     install -Dm644 contrib/omarchy/osnip.lua ~/.config/hypr/osnip.lua
--
-- Then load it from ~/.config/hypr/hyprland.lua, *after* the other
-- `require("hypr.*")` lines, so these rules land last and win:
--
--     require("hypr.osnip")
--
-- Hyprland re-reads its config on save. Validate the result with:
--
--     hyprctl reload && hyprctl configerrors
--
-- Tested on Omarchy 4.x / Hyprland 0.56.

--------------------------------------------------------------------
-- Window rules
--------------------------------------------------------------------
-- The daemon stamps every pin window with the Wayland app-id
-- "osnip-pin" (crates/osnip-daemon/src/app.rs :: PIN_APP_ID).
-- Hyprland surfaces a Wayland app-id as `class`.

o.window("^osnip-pin$", {
  -- A pin is an overlay; it must never join the tiling layout.
  -- Floating (as opposed to Hyprland-`pin`ned) is also what makes pins
  -- per-workspace: a pin opens on whichever workspace was active when
  -- you captured, and stays there when you switch away. Every
  -- workspace keeps its own set of pins.
  float = true,

  -- Omarchy tags every window `default-opacity` and renders it at
  -- "0.985 0.96". A pin *is* a screenshot — translucency would blend
  -- the captured pixels with whatever sits underneath. Because this
  -- file loads after Omarchy's opacity rule, the later value wins.
  opacity = "1 1",

  -- Dimming a reference image defeats the point of pinning it.
  no_dim = true,

  -- Don't round off the captured corners if you've set a global
  -- `decoration.rounding` in ~/.config/hypr/looknfeel.lua.
  rounding = 0,
})

-- Optional: make *every* pin follow you across workspaces instead of
-- staying on the one it was captured on. Hyprland's `pin` also keeps a
-- window visible over a focused fullscreen window, which plain floating
-- does not -- that is the trade-off for per-workspace pins.
--
-- o.window("^osnip-pin$", { pin = true })

-- Optional: anchor new pins to the top-right corner instead of letting
-- Hyprland place them, so a burst of captures doesn't stack on itself.
-- `move` takes Hyprland's layout expressions.
--
-- o.window("^osnip-pin$", {
--   move = { "(monitor_w-window_w-40)", "(monitor_h*0.04)" },
-- })

-- Optional: let a pin appear without stealing focus. Off by default,
-- because the in-window shortcuts (Ctrl+C, Ctrl+S, [ ] H V) only reach
-- a focused pin.
--
-- o.window("^osnip-pin$", { no_initial_focus = true })

-- Optional: drop the border so the pin is nothing but captured pixels.
--
-- o.window("^osnip-pin$", { border_size = 0 })

--------------------------------------------------------------------
-- Keybindings
--------------------------------------------------------------------
-- Stock Omarchy binds SUPER+SHIFT+S to the Google Maps web app, so it
-- has to be unbound before it can be reused. Google Maps stays
-- reachable from the Omarchy menu (SUPER+SPACE).
hl.unbind("SUPER + SHIFT + S")
o.bind("SUPER + SHIFT + S", "Pin a screen region", "osnip capture")

-- Pinning the clipboard image and closing every pin are one click away
-- in the Osnip panel (and on the bar icon: middle-click pins the
-- clipboard). Bind them here too only if you want them on the keyboard:
--
-- o.bind("SUPER + SHIFT + V", "Pin the clipboard image", "osnip clipboard")
-- hl.unbind("SUPER + SHIFT + X")
-- o.bind("SUPER + SHIFT + X", "Close every pin", "osnip close-all")

-- Optional: promote just the focused pin to follow you across
-- workspaces, leaving the rest per-workspace. SUPER+ALT+P is unbound in
-- stock Omarchy. This toggles, so the same key sends it back.
--
-- Omarchy's SUPER+O ("Pop window out") is NOT a substitute: it toggles
-- tiling and resizes the window to 1300x900, which destroys the pin's
-- size and aspect ratio.
--
-- o.bind("SUPER + ALT + P", "Pin/unpin across workspaces", hl.dsp.window.pin())

--------------------------------------------------------------------
-- Daemon
--------------------------------------------------------------------
-- Nothing is required here: the CLI auto-spawns osnip-daemon when
-- it finds no socket. Starting it at login is worth it if you run the
-- Osnip bar plugin, which can only show a live pin count while the
-- daemon is up. Either uncomment this (it wraps the daemon in a uwsm
-- scope, matching how Omarchy launches everything else)…
--
-- o.launch_on_start("osnip-daemon")
--
-- …or install the systemd user unit in contrib/osnip-daemon.service.
-- Do one or the other, not both.
