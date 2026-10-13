-- EmbyMini 选集菜单：EmbyMini 每次换集时下发整季集名，这里只负责显示，选中后把下标回传给 EmbyMini。
-- 不用 mpv 自带的播放列表菜单：还没播过的条目在那里显示成直链 URL，而且列表里只有已加载的几集。
local input = require "mp.input"
local utils = require "mp.utils"
local titles, cur = {}, 1

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
