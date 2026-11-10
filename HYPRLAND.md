# Hyprland setup for Nexus

Everything Nexus needs in `~/.config/hypr/hyprland.lua`. `./install.sh` installs the binaries but never edits this file, so add these sections yourself, then run `hyprctl reload`. Sections 1 to 6 go at the top level of the file, in this order.

**Requires Hyprland 0.55 or newer with the Lua config.** A `hyprland.conf` is not enough: Nexus calls `hl.dsp.*` dispatchers and sends `hl.config` to the compositor at runtime. Tested on 0.57.1.

## 1. Environment

Shortcuts inherit Hyprland's `PATH`, not your shell's, and `nexus` is installed in `~/.local/bin`. Without this, every shortcut below silently does nothing.

```lua
do
    local bin = os.getenv("HOME") .. "/.local/bin"
    local path = os.getenv("PATH") or "/usr/local/bin:/usr/bin"
    if not (":" .. path .. ":"):find(":" .. bin .. ":", 1, true) then
        hl.env("PATH", bin .. ":" .. path)
    end
end

-- The file chooser portal is only used under this desktop name.
hl.env("XDG_CURRENT_DESKTOP", "Hyprland")
hl.env("XDG_SESSION_TYPE", "wayland")
hl.env("XDG_SESSION_DESKTOP", "Hyprland")
```

### Without systemd (Artix, Void…)

Nothing exports the session bus address at login. GTK apps recover it, but Chromium and Electron apps lose notifications and portals. Skip this block on systemd.

```lua
do
    -- Hyprland reaps its own children, so os.execute never sees an exit code.
    local function alive(address)
        local p = io.popen(("dbus-send --bus='%s' --dest=org.freedesktop.DBus --print-reply "
            .. "/org/freedesktop/DBus org.freedesktop.DBus.GetId 2>/dev/null"):format(address))
        local reply = p:read("a")
        pcall(p.close, p)
        return reply:find("string") ~= nil
    end
    local current = os.getenv("DBUS_SESSION_BUS_ADDRESS") or ""
    if not current:find("^unix:") then
        local runtime = os.getenv("XDG_RUNTIME_DIR") or "/tmp"
        local candidates = { "unix:path=" .. runtime .. "/bus" }
        -- A bus started by X11 autolaunch before this config exported one.
        local id = io.open("/etc/machine-id")
        if id then
            local file = io.open(os.getenv("HOME") .. "/.dbus/session-bus/" .. id:read("l") .. "-0")
            id:close()
            if file then
                local address = file:read("a"):match("DBUS_SESSION_BUS_ADDRESS='([^']+)'")
                file:close()
                if address then table.insert(candidates, address) end
            end
        end
        local address
        for _, candidate in ipairs(candidates) do
            if alive(candidate) then
                address = candidate
                break
            end
        end
        if not address then
            address = candidates[1]
            os.execute(("rm -f '%s/bus'; dbus-daemon --session --address='%s' --fork --nopidfile --syslog-only")
                :format(runtime, address))
        end
        hl.env("DBUS_SESSION_BUS_ADDRESS", address)
    end
end
```

## 2. Startup

`nexus-session` starts the daemon and the shell. Make it the first command so the bar appears at once.

```lua
hl.on("hyprland.start", function()
    hl.exec_cmd("~/.local/bin/nexus-session")
end)
```

On systemd you can instead run `systemctl --user enable --now nexusd`, but `nexus shell` must still be started from here.

## 3. Blur and session lock

Turn blur on here. Nexus sends the rest of the blur settings itself, according to the style chosen in Settings › Appearance.

```lua
hl.config({
    decoration = {
        blur = { enabled = true, new_optimizations = true, popups = true },
    },
    misc = {
        -- A crashed locker can be started again from a TTY.
        allow_session_lock_restore = true,
    },
})
```

## 4. Window rules

Without these, the settings window and Paint open tiled instead of floating.

```lua
hl.window_rule({
    name       = "nexus-control-float",
    match      = { class = "^(io\\.github\\.vitrixxl\\.Nexus)$", title = "^(Nexus)$" },
    float      = true,
    size       = "900 640",
    move       = "monitor_w/2-450 monitor_h/2-320",
    -- Dims the rest of the screen. Nexus closes the window on a click there.
    dim_around = true,
})
hl.window_rule({
    name  = "nexus-paint-float",
    match = { class = "^(io\\.github\\.vitrixxl\\.NexusPaint)$" },
    float = true,
    size  = "monitor_w*0.9 monitor_h*0.8",
    move  = "monitor_w*0.05 monitor_h*0.1",
})
```

## 5. Layer rules

These give Nexus surfaces their glass. `no_anim` matters: the surfaces animate themselves, and a compositor fade on top makes them stutter.

```lua
for _, namespace in ipairs({
    "nexus-bar", "nexus-panel", "nexus-drawer", "nexus-notifications",
    "nexus-clipboard", "nexus-dock", "nexus-osd",
}) do
    hl.layer_rule({
        name         = namespace .. "-glass",
        match        = { namespace = "^" .. namespace .. "$" },
        blur         = true,
        ignore_alpha = 0.16,
        no_anim      = true,
    })
end
-- Invisible surface that closes a panel on an outside click. Never blur it.
hl.layer_rule({
    name    = "nexus-click-away-no-anim",
    match   = { namespace = "^nexus-click-away$" },
    no_anim = true,
})
-- The screenshot picker fades itself. A compositor fade would end up in the capture.
hl.layer_rule({
    name    = "nexus-screenshot-no-anim",
    match   = { namespace = "^nexus-screenshot$" },
    no_anim = true,
})
```

## 6. Shortcuts

These are the keys used by the author's setup. Change any of them, the commands are what matters.

```lua
local function run(cmd) return hl.dsp.exec_cmd(cmd) end

hl.bind("SUPER + Space", run("nexus launcher"))
hl.bind("SUPER + D", run("nexus launcher"))
hl.bind("SUPER + E", run("nexus-files"))
hl.bind("SUPER + N", run("nexus control"))
hl.bind("SUPER + P", run("nexus paint"))
hl.bind("SUPER + W", run("nexus wifi"))
hl.bind("SUPER + ALT + N", run("nexus"))
hl.bind("SUPER + ALT + B", run("nexus bluetooth"))
hl.bind("SUPER + ALT + A", run("nexus sound"))
hl.bind("SUPER + ALT + P", run("nexus power"))
hl.bind("SUPER + ALT + L", run("nexus lock"))
hl.bind("SUPER + SHIFT + V", run("nexus clipboard"))
hl.bind("CTRL + ALT + Delete", run("nexus power"))

-- Screenshots and recording. Pressing the key again cancels or stops.
hl.bind("Print", run("nexus screenshot --live --save"))
hl.bind("CTRL + Print", run("nexus screenshot screen --save"))
hl.bind("ALT + Print", run("nexus screenshot window --save"))
hl.bind("SUPER + SHIFT + S", run("nexus screenshot"))
hl.bind("SUPER + SHIFT + R", run("nexus screenshot --record"))
```

## Check it

```sh
Hyprland --verify-config -c ~/.config/hypr/hyprland.lua
hyprctl reload
```

After changing the startup section, log out and back in. To restart only Nexus from a running session:

```sh
pkill -x nexusd; pkill -x nexus
hyprctl dispatch 'hl.dsp.exec_cmd("~/.local/bin/nexus-session")'
```

Start it through Hyprland as above, not directly from a terminal, or the terminal's environment leaks into every app opened from the launcher.
