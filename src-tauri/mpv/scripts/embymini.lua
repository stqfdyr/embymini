-- EmbyMini 选集菜单：EmbyMini 每次换集时下发整季集名，这里只负责显示，选中后把下标回传给 EmbyMini。
-- 不用 mpv 自带的播放列表菜单：还没播过的条目在那里显示成直链 URL，而且列表里只有已加载的几集。
-- 另外：暂停时光标不自动隐藏（控制栏暂停时常显，见 modernz.conf 的 keeponpause），继续播放后恢复原设置。
local input = require "mp.input"
local utils = require "mp.utils"
local titles, cur = {}, 1

local autohide -- 暂停前的 cursor-autohide（个人配置可能改过，不写死）
mp.observe_property("pause", "bool", function(_, paused)
    if paused then
        autohide = autohide or mp.get_property("cursor-autohide")
        mp.set_property("cursor-autohide", "no")
    elseif autohide then
        mp.set_property("cursor-autohide", autohide)
        autohide = nil
    end
end)

mp.register_script_message("episodes", function(json, at)
    titles = utils.parse_json(json) or {}
    cur = (tonumber(at) or 0) + 1
end)

mp.register_script_message("menu", function()
    if #titles == 0 then
        return
    end
    input.select({
        prompt = "选集：",
        items = titles,
        default_item = cur,
        submit = function(i)
            if i ~= cur then
                mp.commandv("script-message", "embymini-jump", tostring(i - 1))
            end
        end,
    })
end)
