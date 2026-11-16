-- Screenshots of the bundled mpv config without a server: ModernZ controls, next episode ready (green bolt),
-- episode menu (CJK and icon fonts).
-- mpv --config-dir=resources/mpv --script=tools/mpv-shot.lua --force-window=yes "av://lavfi:testsrc2=duration=60:size=1920x1080"
-- Writes to $SHOT_DIR (default: current directory) and quits. Used by Windows CI and local Xvfb.
local dir = os.getenv("SHOT_DIR") or "."
local function shot(name) mp.commandv("screenshot-to-file", dir .. "/" .. name .. ".png", "window") end
local done = false

mp.register_event("file-loaded", function()
    if done then return end
    done = true
    -- ModernZ shows itself at start and then fades out, ignoring show requests during the fade: pin it before that
    mp.commandv("script-message-to", "modernz", "osc-visibility", "always", "no_osd")
    mp.add_timeout(3.5, function()
        shot("1-modernz")
        mp.commandv("script-message", "embymini-next-ready", "yes")
        mp.add_timeout(0.5, function() shot("1b-next-ready") end)
        mp.commandv("script-message-to", "embymini", "episodes",
            '["S01E01 第一集：中文与 English 混排","S01E02 第二集","S01E03 第三集"]', "0")
        mp.commandv("script-message-to", "embymini", "menu")
        mp.add_timeout(1.5, function()
            shot("2-episode-menu")
            mp.command("quit")
        end)
    end)
end)
