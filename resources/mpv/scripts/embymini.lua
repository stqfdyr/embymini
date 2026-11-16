-- EmbyMini episode menu: EmbyMini sends the season's episode names on every switch; this only shows them and sends
-- the picked index back. mpv's own playlist menu shows unplayed entries as stream URLs and only has loaded episodes.
-- Also keeps the cursor visible while paused (the controls stay too, keeponpause in modernz.conf).
local input = require "mp.input"
local utils = require "mp.utils"
local titles, cur = {}, 1

local autohide -- cursor-autohide before the pause (a personal config may have changed it)
mp.observe_property("pause", "bool", function(_, paused)
    if paused then
        autohide = autohide or mp.get_property("cursor-autohide")
        mp.set_property("cursor-autohide", "no")
    elseif autohide then
        mp.set_property("cursor-autohide", autohide)
        autohide = nil
    end
end)

-- `at` is EmbyMini's 0-based queue index
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
