-- 不连服务器，单独检查自带 mpv 配置的实际渲染：ModernZ 控制条、选集菜单（中文字体、图标字体）。
-- 用法：mpv --config-dir=src-tauri/mpv --script=tools/mpv-shot.lua --force-window=yes "av://lavfi:testsrc2=duration=60:size=1920x1080"
-- 截图写到环境变量 SHOT_DIR（默认当前目录），拍完自动退出。Windows CI 诊断与本机 Xvfb 都用它。
local dir = os.getenv("SHOT_DIR") or "."
local function shot(name) mp.commandv("screenshot-to-file", dir .. "/" .. name .. ".png", "window") end
local done = false

mp.register_event("file-loaded", function()
    if done then return end
    done = true
    -- 开播就切到常显：ModernZ 开播时先显示、随后自动淡出，淡出过程中再要求显示会被忽略，
    -- 所以要赶在自动隐藏之前关掉它，截图时控制条才一定在
    mp.commandv("script-message-to", "modernz", "osc-visibility", "always", "no_osd")
    mp.add_timeout(2, function()
        mp.add_timeout(1.5, function()
            shot("1-modernz")
            mp.commandv("script-message-to", "embymini", "episodes",
                '["S01E01 第一集：中文与 English 混排","S01E02 第二集","S01E03 第三集"]', "0")
            mp.commandv("script-message-to", "embymini", "menu")
            mp.add_timeout(1.5, function()
                shot("2-episode-menu")
                mp.command("quit")
            end)
        end)
    end)
end)
