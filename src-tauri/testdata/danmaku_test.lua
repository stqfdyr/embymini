-- Runs mpv/scripts/danmaku.lua against a fake `mp` and checks what it draws and sends.
-- Started by the player test `danmaku_script` (path in --script-opts=dmtest-src=…); quits mpv with 0 on success.
local utils = require "mp.utils"
local real = mp

local fake = { time = 100, props = {}, handlers = {}, events = {}, observers = {}, overlays = {}, sent = {}, bindings = {} }
local m = { msg = { verbose = function() end } }
function m.get_time() return fake.time end
function m.get_property(k, d) local v = fake.props[k]; if v == nil then return d end return v end
m.get_property_number, m.get_property_native = m.get_property, m.get_property
function m.set_property(k, v) fake.props[k] = v end
m.set_property_native = m.set_property
function m.commandv(...) fake.sent[#fake.sent + 1] = { ... } end
function m.osd_message(s) fake.osd = s end
function m.register_script_message(name, fn) fake.handlers[name] = fn end
function m.register_event(name, fn) fake.events[name] = fn end
function m.observe_property(name, _, fn) fake.observers[name] = fn end
function m.add_forced_key_binding(_, name, fn) fake.bindings[name] = fn end
function m.remove_key_binding(name) fake.bindings[name] = nil end
function m.add_periodic_timer(_, fn)
    local t = { fn = fn, timeout = 0 }
    function t.kill() end
    function t.resume() end
    return t
end
function m.create_osd_overlay()
    local o = { data = "", updates = 0 }
    function o.update(self) self.updates = self.updates + 1 end
    function o.remove(self) self.data = "" end
    fake.overlays[#fake.overlays + 1] = o
    return o
end
local input = {
    get = function(o) fake.input = o end,
    select = function(o) fake.select = o end,
    terminate = function() end,
}
local env = setmetatable({
    mp = m,
    require = function(name) if name == "mp.input" then return input end return require(name) end,
}, { __index = _G })

local failures = 0
local function check(ok, what)
    if not ok then
        failures = failures + 1
        print("FAIL: " .. what)
    end
end

-- Helpers ---------------------------------------------------------------------------------------------------
local function send(name, v, ...) fake.handlers[name](type(v) == "string" and v or utils.format_json(v), ...) end

-- Lines of an overlay as { an, x, y, text }.
local function lines(o)
    local out = {}
    for line in o.data:gmatch("[^\n]+") do
        local an, x, y = line:match("\\an(%d)\\pos%((%-?[%d.]+),(%-?[%d.]+)%)")
        out[#out + 1] = { an = tonumber(an), x = tonumber(x), y = tonumber(y), text = line:match("}([^}]*)$") or "" }
    end
    return out
end

-- Renders the danmaku overlay as it looks at video time t (paused, as after a seek).
local function at(t)
    fake.props["time-pos"] = t
    fake.observers["time-pos"](nil, t)
    fake.events["playback-restart"]()
    return lines(fake.overlays[1])
end

local function find(ls, text)
    for _, l in ipairs(ls) do if l.text == text then return l end end
end

local function sent(name)
    local hits = {}
    for _, c in ipairs(fake.sent) do
        if c[2] == name then hits[#hits + 1] = c end
    end
    return hits
end

local function panel() return lines(fake.overlays[2]) end

-- Clicks the centre of the panel text `label` (fn = the button's event: down, then up).
local function click(label)
    local l = find(panel(), label)
    check(l ~= nil, "panel has " .. label)
    if not l then return end
    fake.observers["mouse-pos"](nil, { x = l.x, y = l.y })
    fake.bindings["dm-panel-MBTN_LEFT"]({ event = "down" })
    if fake.bindings["dm-panel-MBTN_LEFT"] then fake.bindings["dm-panel-MBTN_LEFT"]({ event = "up" }) end
end

local function episode(item, list)
    send("status", { kind = "loading", text = "", item = item, start = true })
    send("load", { item = item, title = "某剧", list = list })
end

local function state() return fake.props["user-data/embymini/danmaku"] end

-- Tests -----------------------------------------------------------------------------------------------------
local function run()
    fake.observers["osd-dimensions"](nil, { w = 1280, h = 720 })
    fake.observers["core-idle"](nil, true)
    send("init", { style = {}, words = { "Spoiler" } })

    -- Scrolling: same-time comments take separate rows; with all 13 rows (half of 720 px) taken the rest are dropped
    local list = {}
    for i = 1, 15 do list[i] = { 10, 1, 16777215, "c" .. i, 1.1 } end
    episode("A", list)
    local ls = at(10.5)
    check(#ls == 13, "13 rows on half screen, got " .. #ls)
    local ys = {}
    for _, l in ipairs(ls) do
        check(not ys[l.y], "row y " .. l.y .. " used once")
        ys[l.y] = true
    end
    check(state().count == 15 and state().shown == 15, "user-data counts")

    -- Merging, blocked words and types, colours, escaping, top / bottom placement
    episode("A", {
        { 20, 1, 16777215, "哈哈", 2 }, { 22, 1, 16777215, "哈哈", 2 }, { 25, 1, 16777215, "哈哈", 2 },
        { 26, 1, 16777215, "big SPOILER here", 8.8 },
        { 26, 1, 16777215, "{\\b1}x\\N", 4.4 },
        { 26, 5, 16711680, "top", 1.65 },
        { 26, 4, 16777215, "bottom", 3.3 },
        { 41, 1, 16777215, "哈哈", 2 },
    })
    check(state().count == 8 and state().shown == 5, "merged / blocked counts")
    ls = at(26.2)
    check(find(ls, "哈哈 ×3") ~= nil, "merged ×3 shown")
    check(find(ls, "｛＼b1｝x＼N") ~= nil, "ASS escaped")
    local top, bottom = find(ls, "top"), find(ls, "bottom")
    check(top and top.an == 8 and top.x == 640 and top.y == 0, "top centred on row 1")
    check(bottom and bottom.an == 2 and bottom.x == 640 and bottom.y > 690, "bottom centred at the bottom")
    check(fake.overlays[1].data:find("\\1c&H0000FF&", 1, true) ~= nil, "red as BGR")
    ls = at(41.5)
    check(find(ls, "哈哈") ~= nil and not find(ls, "哈哈 ×3"), "outside the merge window shown again")

    -- Nothing on screen: the empty overlay is not resent
    at(200)
    local n = fake.overlays[1].updates
    at(201)
    check(fake.overlays[1].updates == n, "empty overlay not resent")

    -- Messages for another item (sent before the episode changed) are dropped
    send("load", { item = "B", title = "别的", list = {} })
    check(state().count == 8, "load for another item dropped")
    send("status", { kind = "error", text = "x", item = "B" })
    check(state().kind == "ok", "status for another item dropped")
end

local ok, err = pcall(function()
    local chunk = assert(loadfile(real.get_opt("dmtest-src"), "t", env))
    chunk()
    run()
end)
if not ok then
    failures = failures + 1
    print("ERROR: " .. tostring(err))
end
print(failures == 0 and "danmaku_test: ok" or ("danmaku_test: " .. failures .. " failed"))
real.commandv("quit", failures == 0 and "0" or "1")
