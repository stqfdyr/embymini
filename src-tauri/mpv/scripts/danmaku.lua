-- EmbyMini danmaku: layout, per-frame drawing, the settings panel and manual matching.
-- EmbyMini fetches the data and sends it over IPC (messages at the end of this file).
--
-- Not a subtitle track: mpv renders subtitles at the current video frame's time, so on a 24 fps source scrolling
-- text moves 24 times a second and stutters on a high refresh screen. Instead an osd-overlay is redrawn by a timer at
-- the display rate, with time extrapolated from the wall clock and slowly calibrated to time-pos; mpv redraws the OSD
-- between video frames whenever it changes, so scrolling is as smooth as the display.
--
-- Layout as on common video sites: a scrolling comment shares a row only if it cannot catch up with the one ahead,
-- and is dropped when no row is free (no overlap); top / bottom comments stay centred for FIXED seconds.
-- The display area limits scrolling and top rows; bottom rows always stack up from the bottom edge.

local utils = require "mp.utils"
local input = require "mp.input"
local assdraw = require "mp.assdraw"

local DEFAULT = {
    on = true, opacity = 0.8, area = 0.5, scale = 1, speed = 3, bold = true, border = "heavy", merge = true,
    noScroll = false, noTop = false, noBottom = false, noColor = false,
}
local SPEEDS = { 13, 10, 8, 6.5, 5 }            -- seconds to cross the screen, per speed setting 1–5
local SPEED_OPTS = { { 1, "极慢" }, { 2, "较慢" }, { 3, "适中" }, { 4, "较快" }, { 5, "极快" } }
local AREAS = { { 0.25, "1/4" }, { 0.5, "半屏" }, { 0.75, "3/4" }, { 1, "满屏" } }
local BORDER_OPTS = { { "heavy", "重墨" }, { "outline", "描边" }, { "shadow", "投影" }, { "bold", "粗体" } } -- panel row: the borders plus the bold toggle
local FIXED = 5                                 -- seconds a top / bottom comment stays
local MERGE_WINDOW = 15                         -- repeats within this many seconds of the first are merged into it
local FONT_RATIO, MIN_FONT, LINE = 0.032, 14, 1.2 -- font size = window height × ratio × scale; row = LINE × font
local BOTTOM_AREA = 0.5                         -- bottom comments use at most this part of the height
local CATCHUP, SNAP = 0.02, 0.5                 -- per frame the clock moves this part of the way to time-pos; jumps past SNAP s
local REWIND = 0.3                              -- time going back more than this (s) means a seek: lay out again
local FONT = package.config:sub(1, 1) == "\\" and "Microsoft YaHei" or "sans-serif"
local ACCENT = "ECAE00"                         -- ASS colours are BGR

local style = {}
for k, v in pairs(DEFAULT) do style[k] = v end
local words = {}
local offset = 0                                -- seconds, positive = later; this episode only

local raw = nil                                 -- this episode: { t, mode, color, text, w (em) }
local list = {}                                 -- filtered and merged
local src = { kind = "idle", text = "" }        -- panel status line; kind = idle / loading / ok / none / error / setup
                                                -- (ok also has title and note, so a long title is cut but not the note)
local want = nil                                -- the episode's item id (first status of an episode has start); others are stale

-- Screen and clock ------------------------------------------------------------------------------------------
local W, H = 0, 0
local fps = 60
local clock = { pos = 0, wall = 0, rate = 1, run = false }
local tpos = nil                                -- time-pos as observed, not read synchronously each frame

local function now()
    if clock.run then return clock.pos + (mp.get_time() - clock.wall) * clock.rate end
    return clock.pos
end

local function resync()
    tpos = mp.get_property_number("time-pos") or tpos
    clock.pos = tpos or clock.pos
    clock.wall = mp.get_time()
end

-- time-pos only changes with video frames (a sawtooth at 24 fps): following it directly would stutter again
local function calibrate()
    if not tpos then return end
    local err = tpos - now()
    if math.abs(err) > SNAP then return resync() end
    clock.pos = clock.pos + err * CATCHUP
end

-- Data ------------------------------------------------------------------------------------------------------
-- Width in em, estimated per character class (measuring needs a libass pass). EmbyMini sends it precomputed
-- (same rule as em_width in danmaku.rs); here it is only for the merged " ×N" suffix.
local UTF8_CHAR = "[%z\1-\127\194-\244][\128-\191]*"

local function em_width(s)
    local w = 0
    for ch in s:gmatch(UTF8_CHAR) do
        local b = ch:byte()
        if b < 128 then w = w + 0.55 elseif b < 0xE0 then w = w + 0.6 elseif b < 0xF0 then w = w + 1 else w = w + 1.1 end
    end
    return w
end

-- s cut to at most `em` wide, ending in "…" when cut
local function fit(s, em)
    if em_width(s) <= em then return s end
    local out, w = {}, 1                        -- room for the ellipsis
    for ch in s:gmatch(UTF8_CHAR) do
        w = w + em_width(ch)
        if w > em then break end
        out[#out + 1] = ch
    end
    return table.concat(out) .. "…"
end

-- Braces would open an override block, backslashes an escape, a newline a new event (events are joined by \n)
local function ass_escape(s)
    return (s:gsub("\\", "＼"):gsub("{", "｛"):gsub("}", "｝"):gsub("[\r\n]", " "))
end

local function blocked(c)
    if (c.mode == 1 and style.noScroll) or (c.mode == 5 and style.noTop) or (c.mode == 4 and style.noBottom) then
        return true
    end
    if style.noColor and c.color ~= 0xFFFFFF then return true end
    if #words > 0 then
        local low = c.text:lower()
        for _, k in ipairs(words) do
            if low:find(k, 1, true) then return true end
        end
    end
    return false
end

-- Rebuilds the list to show from the settings (blocked types, merging, blocked words)
local function rebuild()
    list = {}
    if not raw then return end
    local first = {}                            -- text → its first item within the merge window
    for _, c in ipairs(raw) do
        if not blocked(c) then
            local f = style.merge and first[c.text]
            if f and c.t - f.t <= MERGE_WINDOW then
                f.n = f.n + 1
            else
                local item = { t = c.t, mode = c.mode, color = c.color, text = c.text, w = c.w, n = 1 }
                first[c.text] = item
                list[#list + 1] = item
            end
        end
    end
    for _, item in ipairs(list) do
        local text = item.text
        if item.n > 1 then
            local suffix = " ×" .. item.n
            text, item.w = text .. suffix, item.w + em_width(suffix)
        end
        -- What never changes (colours, text) is built once; each frame only adds position and style
        local c = item.color
        local r, g, b = math.floor(c / 65536) % 256, math.floor(c / 256) % 256, c % 256
        item.tail = string.format("\\1c&H%02X%02X%02X&\\3c&H%s&}%s", b, g, r,
            r + g + b < 120 and "FFFFFF" or "000000", ass_escape(text))
    end
end

-- Layout ----------------------------------------------------------------------------------------------------
local active = {}
local rows = { [1] = {}, [4] = {}, [5] = {} }   -- per mode, the last comment in each row
local nexti = 1
local last_t = 0

local function fs() return math.max(MIN_FONT, H * FONT_RATIO * style.scale) end
local function row_h() return fs() * LINE end
local function duration() return SPEEDS[style.speed] or 8 end

local function first_index(t)                   -- binary search: first item with time >= t
    local lo, hi = 1, #list + 1
    while lo < hi do
        local mid = math.floor((lo + hi) / 2)
        if list[mid].t < t then lo = mid + 1 else hi = mid end
    end
    return lo
end

-- Clears the screen and lays out again from time t: whatever appeared within the last D seconds comes back
local function relayout(t)
    active, rows = {}, { [1] = {}, [4] = {}, [5] = {} }
    nexti = first_index(t - offset - duration())
    last_t = t
end

local function place(item, t0)
    local f = fs()
    local w = item.w * f
    local nscroll = math.max(1, math.floor(H * style.area / row_h()))
    if item.mode == 1 then
        local D = duration()
        local v = (W + w) / D
        for r = 1, nscroll do
            local last = rows[1][r]
            -- the one ahead is fully on screen, and this one cannot catch it before it leaves
            if not last or ((t0 - last.t0) * last.v >= last.w + f and t0 + W / v >= last.t0 + D) then
                local a = { item = item, row = r, t0 = t0, w = w, v = v, ends = t0 + D }
                rows[1][r] = a
                active[#active + 1] = a
                return
            end
        end
    else
        local n = item.mode == 5 and nscroll or math.max(1, math.floor(H * BOTTOM_AREA / row_h()))
        for r = 1, n do
            local last = rows[item.mode][r]
            if not last or t0 >= last.ends then
                local a = { item = item, row = r, t0 = t0, w = w, ends = t0 + FIXED }
                rows[item.mode][r] = a
                active[#active + 1] = a
                return
            end
        end
    end
end

-- Drawing ---------------------------------------------------------------------------------------------------
local overlay = mp.create_osd_overlay("ass-events")
overlay.z = -10                                 -- below the control bar

local function head()
    local f = fs()
    local a = string.format("%02X", math.floor((1 - style.opacity) * 255 + 0.5))
    -- One layer per border style: each extra layer is another bitmap to pack and upload every frame
    local bord, shad = 0, 0
    if style.border == "heavy" then bord = f * 0.07
    elseif style.border == "outline" then bord = f * 0.04
    else shad = f * 0.06 end
    return string.format("\\fn%s\\fs%.1f\\b%d\\bord%.2f\\shad%.2f\\1a&H%s&\\3a&H%s&\\4a&H%s&",
        FONT, f, style.bold and 1 or 0, math.max(bord, bord > 0 and 1 or 0), shad, a, a, a)
end

local function draw(t)
    if not style.on or not raw or W == 0 then
        if overlay.data ~= "" then overlay.data = ""; overlay:update() end
        return
    end
    local tt = t - offset
    if t < last_t - REWIND then relayout(t) end
    last_t = t
    local D = duration()
    while nexti <= #list and list[nexti].t <= tt do
        local item = list[nexti]
        if item.t >= tt - D then place(item, item.t + offset) end
        nexti = nexti + 1
    end
    local h, rh, out, keep = head(), row_h(), {}, {}
    local bottom = H - rh * 0.3
    for _, a in ipairs(active) do
        if t < a.ends then
            keep[#keep + 1] = a
            local pos
            if a.item.mode == 1 then
                -- Whole pixels: libass caches bitmaps per subpixel offset, so rounding makes every frame a cache hit
                pos = string.format("\\an7\\pos(%d,%d)", math.floor(W - (t - a.t0) * a.v + 0.5), math.floor((a.row - 1) * rh + 0.5))
            elseif a.item.mode == 5 then
                pos = string.format("\\an8\\pos(%d,%d)", math.floor(W / 2), math.floor((a.row - 1) * rh + 0.5))
            else
                pos = string.format("\\an2\\pos(%d,%d)", math.floor(W / 2), math.floor(bottom - (a.row - 1) * rh + 0.5))
            end
            out[#out + 1] = "{" .. pos .. h .. a.item.tail
        end
    end
    active = keep
    local data = table.concat(out, "\n")
    if data == "" and overlay.data == "" then return end -- every update makes mpv redraw the OSD
    overlay.res_x, overlay.res_y = W, H
    overlay.data = data
    overlay:update()
end

local timer = mp.add_periodic_timer(1 / 60, function()
    calibrate()
    draw(now())
end)
timer:kill()

-- Draws every frame only while playing, on, loaded and with a window; otherwise draws once
local function refresh()
    local run = clock.run and style.on and raw ~= nil and W > 0
    timer:kill()
    if run then
        timer.timeout = 1 / math.max(24, math.min(fps, 240)) -- takes effect on resume
        timer:resume()
    end
    draw(now())
end

-- Settings panel --------------------------------------------------------------------------------------------
local panel = mp.create_osd_overlay("ass-events")
panel.z = 1000
local open = false
local hits = {}
local drag = nil
local mouse = { x = -1, y = -1 }
local autohide = nil
local box = nil                                 -- panel bounds {x0, y0, x1, y1}

local function announce()                       -- for ModernZ's danmaku button
    mp.commandv("script-message", "embymini-danmaku-on", style.on and "yes" or "no")
end

local function save()
    mp.commandv("script-message", "embymini-danmaku-style", utils.format_json(style))
    announce()
end

local function toast(text) mp.osd_message("弹幕：" .. text, 4) end

-- Current state in user-data, for EmbyMini's end-to-end test and the console
local function publish()
    mp.set_property_native("user-data/embymini/danmaku", {
        on = style.on, kind = src.kind, text = src.text, count = raw and #raw or 0, shown = #list,
    })
end

local function shape(out, x0, y0, x1, y1, r, color, alpha)
    local a = assdraw.ass_new()
    a:draw_start()
    a:round_rect_cw(x0, y0, x1, y1, r)
    a:draw_stop()
    out[#out + 1] = string.format("{\\an7\\pos(0,0)\\bord0\\shad0\\1c&H%s&\\1a&H%s&}", color, alpha or "00") .. a.text
end

local function text(out, x, y, an, size, s, color, alpha)
    out[#out + 1] = string.format("{\\an%d\\pos(%.1f,%.1f)\\bord0\\shad0\\fn%s\\fs%.1f\\b0\\1c&H%s&\\1a&H%s&}%s",
        an, x, y, FONT, size, color or "FFFFFF", alpha or "00", s)
end

local function hover(x0, y0, x1, y1)
    return mouse.x >= x0 and mouse.x <= x1 and mouse.y >= y0 and mouse.y <= y1
end

local function hit(x0, y0, x1, y1, fn, is_drag)
    hits[#hits + 1] = { x0 = x0, y0 = y0, x1 = x1, y1 = y1, fn = fn, drag = is_drag }
end

local function after(change)                    -- "rebuild" = filtering changed, "relayout" = layout changed
    if change == "rebuild" then rebuild() end
    if change then relayout(now()) end
    refresh()
end

local function close_panel()
    if not open then return end
    if drag then save() end                     -- Esc mid-drag: the value is applied, so keep it
    open, drag = false, nil
    panel:remove()
    for _, k in ipairs({ "MBTN_LEFT", "MBTN_LEFT_DBL", "ESC" }) do mp.remove_key_binding("dm-panel-" .. k) end
    if autohide then mp.set_property("cursor-autohide", autohide); autohide = nil end
end

-- Manual match: keyword → show → episode; EmbyMini remembers it (later episodes of the season use the same offset).
-- Replies carry the request's argument; anything but the reply to the last lookup ("search\n<keyword>" /
-- "episodes\n<show id>") is a slow earlier one and is dropped. Cleared on episode change.
local asked = nil

local function lookup(name, arg)
    asked = name .. "\n" .. arg
    mp.commandv("script-message", "embymini-danmaku-" .. name, arg)
end

local function manual_match()
    asked = nil                                 -- a late reply to the previous lookup must not replace this prompt
    local title = mp.get_property("media-title", "")
    local kw = title:match("^(.-) S%d+E%d+") or title
    input.get({
        prompt = "搜索弹幕（剧名 / 番剧名）：",
        default_text = kw,
        submit = function(s)
            input.terminate()
            if s ~= "" then
                toast("正在搜索…")
                lookup("search", s)
            end
        end,
    })
end

-- A lookup reply, or nil if it is stale or failed (failures are shown)
local function reply(name, json, arg)
    if asked ~= name .. "\n" .. tostring(arg) then return nil end
    local v = utils.parse_json(json)
    if not v or v.error then
        toast(v and v.error or "搜索失败")
        return nil
    end
    return v
end

local function draw_panel()
    if not open then return end
    hits = {}
    local k = mp.get_property_number("display-hidpi-scale", 1)
    local PW, RH = 380, 36
    local rows_n = 10
    k = math.min(k, (H - 40) / (RH * rows_n + 24), (W - 32) / PW)
    local pw, rh, pad, fsz = PW * k, RH * k, 16 * k, 14 * k
    local ph = rh * rows_n + pad * 1.5
    local x0 = W - pw - 24 * k
    local y0 = math.max(8 * k, H - ph - 130 * k)
    box = { x0, y0, x0 + pw, y0 + ph }
    local out = {}
    shape(out, x0, y0, x0 + pw, y0 + ph, 10 * k, "1E1E1E", "08")
    local lx, cx0, cx1 = x0 + pad, x0 + pad + 74 * k, x0 + pw - pad
    local y = y0 + pad * 0.75

    local function label(s) text(out, lx, y + rh / 2, 4, fsz, s, "C8C8C8") end

    -- A setting changed: `how` as in after(); persist = send to EmbyMini now (sliders wait for release)
    local function set(how, persist)
        if persist then save() end
        after(how)
        draw_panel()
    end

    -- A row of equal options; those with cur(value) true are highlighted
    local function seg(opts, cur, pick, x_from, x_to)
        x_from, x_to = x_from or cx0, x_to or cx1
        local cw = (x_to - x_from) / #opts
        local h = rh * 0.7
        for i, o in ipairs(opts) do
            local a, b = x_from + (i - 1) * cw + 2 * k, x_from + i * cw - 2 * k
            local ty0, ty1 = y + (rh - h) / 2, y + (rh + h) / 2
            local on = cur(o[1])
            shape(out, a, ty0, b, ty1, 5 * k, on and ACCENT or "FFFFFF", on and "00" or (hover(a, ty0, b, ty1) and "C0" or "E0"))
            text(out, (a + b) / 2, y + rh / 2, 5, fsz * 0.93, o[2], "FFFFFF")
            hit(a, ty0, b, ty1, function() pick(o[1]) end)
        end
    end

    local function slider(frac, value_text, change)
        local a, b = cx0 + 6 * k, cx1 - 56 * k
        local cy = y + rh / 2
        shape(out, a, cy - 2 * k, b, cy + 2 * k, 2 * k, "FFFFFF", "B0")
        shape(out, a, cy - 2 * k, a + (b - a) * frac, cy + 2 * k, 2 * k, ACCENT)
        local kx = a + (b - a) * frac
        shape(out, kx - 7 * k, cy - 7 * k, kx + 7 * k, cy + 7 * k, 7 * k, "FFFFFF")
        text(out, cx1, cy, 6, fsz * 0.93, value_text, "C8C8C8")
        hit(a - 8 * k, y, b + 8 * k, y + rh, function(mx) change(math.max(0, math.min(1, (mx - a) / (b - a)))) end, true)
    end

    -- Row 1: title, on / off
    text(out, lx, y + rh / 2, 4, fsz * 1.1, "弹幕设置", "FFFFFF")
    seg({ { true, "开启" }, { false, "关闭" } }, function(v) return style.on == v end, function(v)
        style.on = v; set(nil, true)
    end, cx1 - 120 * k, cx1)
    y = y + rh
    -- Row 2: source / status
    local room = (cx1 - lx) / (fsz * 0.86)      -- in em
    -- Escaped before cutting: full-width ＼｛｝ are wider than what they replace
    local status
    if src.kind == "ok" and raw then
        local tail = ass_escape(src.note) .. string.format(" · %d 条", #raw)
        status = fit(ass_escape(src.title), room - em_width(tail)) .. tail
    else
        status = fit(ass_escape(src.text ~= "" and src.text or (style.on and "播放后自动匹配弹幕" or "弹幕已关闭")), room)
    end
    local bad = src.kind == "error" or src.kind == "setup"
    text(out, lx, y + rh / 2, 4, fsz * 0.86, status, bad and "6B8BFF" or "A0A0A0")
    y = y + rh
    label("屏蔽类型")
    seg({ { "noScroll", "滚动" }, { "noTop", "顶部" }, { "noBottom", "底部" }, { "noColor", "彩色" } },
        function(key) return style[key] end, function(key)
            style[key] = not style[key]; set("rebuild", true)
        end)
    y = y + rh
    label("不透明度")
    slider((style.opacity - 0.1) / 0.9, string.format("%d%%", math.floor(style.opacity * 100 + 0.5)), function(f)
        style.opacity = math.floor((0.1 + f * 0.9) * 20 + 0.5) / 20; set()
    end)
    y = y + rh
    label("显示区域")
    seg(AREAS, function(v) return math.abs(style.area - v) < 0.01 end, function(v)
        style.area = v; set("relayout", true)
    end)
    y = y + rh
    label("弹幕字号")
    slider((style.scale - 0.5) / 1.5, string.format("%d%%", math.floor(style.scale * 100 + 0.5)), function(f)
        style.scale = math.floor((0.5 + f * 1.5) * 20 + 0.5) / 20; set("relayout")
    end)
    y = y + rh
    label("弹幕速度")
    seg(SPEED_OPTS, function(v) return style.speed == v end, function(v)
        style.speed = v; set("relayout", true)
    end)
    y = y + rh
    label("描边")
    seg(BORDER_OPTS, function(v) if v == "bold" then return style.bold end return style.border == v end, function(v)
        if v == "bold" then style.bold = not style.bold else style.border = v end
        set(nil, true)
    end)
    y = y + rh
    label("时间偏移")
    local mid = (cx0 + cx1) / 2
    seg({ { -0.5, "−0.5 秒" }, { 0, string.format("%+.1f 秒", offset) }, { 0.5, "+0.5 秒" } },
        function() return false end, function(d)
            offset = d == 0 and 0 or offset + d; set("relayout")
        end, cx0, mid + 60 * k)
    seg({ { "merge", "合并重复" } }, function() return style.merge end, function()
        style.merge = not style.merge; set("rebuild", true)
    end, mid + 66 * k, cx1)
    y = y + rh
    seg({ { "match", "手动匹配" }, { "reload", "重新加载" }, { "reset", "恢复默认" } }, function() return false end,
        function(v)
            if v == "match" then close_panel(); manual_match()
            elseif v == "reload" then mp.commandv("script-message", "embymini-danmaku-reload")
            else
                for key, val in pairs(DEFAULT) do style[key] = val end
                offset = 0; set("rebuild", true)
            end
        end, lx, cx1)

    panel.res_x, panel.res_y = W, H
    panel.data = table.concat(out, "\n")
    panel:update()
end

local function on_click(e)
    if e.event == "up" then
        if drag then drag = nil; save() end
        return
    end
    if e.event ~= "down" then return end
    local mx, my = mouse.x, mouse.y
    for _, h in ipairs(hits) do
        if mx >= h.x0 and mx <= h.x1 and my >= h.y0 and my <= h.y1 then
            if h.drag then
                drag = h
                h.fn(mx)
            else
                h.fn()
            end
            return
        end
    end
    if box and (mx < box[1] or mx > box[3] or my < box[2] or my > box[4]) then close_panel() end
end

local function open_panel()
    if open then return close_panel() end
    open = true
    mp.add_forced_key_binding("MBTN_LEFT", "dm-panel-MBTN_LEFT", on_click, { complex = true })
    mp.add_forced_key_binding("MBTN_LEFT_DBL", "dm-panel-MBTN_LEFT_DBL", function() end) -- no fullscreen toggle
    mp.add_forced_key_binding("ESC", "dm-panel-ESC", close_panel)
    local p = mp.get_property_native("mouse-pos") or {}
    mouse.x, mouse.y = p.x or -1, p.y or -1
    autohide = autohide or mp.get_property("cursor-autohide")
    mp.set_property("cursor-autohide", "no")
    draw_panel()
end

mp.observe_property("mouse-pos", "native", function(_, p)
    if not open or not p then return end
    mouse.x, mouse.y = p.x, p.y
    if drag then drag.fn(p.x) end
    draw_panel()
end)

-- Manual match replies ---------------------------------------------------------------------------------------
mp.register_script_message("search", function(json, arg)
    local v = reply("search", json, arg)
    local key = asked
    if not v then return end
    if #v == 0 then return toast("没有搜到，换个关键词试试") end
    local items = {}
    for i, a in ipairs(v) do items[i] = string.format("%s（%d 集）", a.title, a.count) end
    mp.osd_message("")
    input.select({
        prompt = "选择剧集：",
        items = items,
        submit = function(i)
            if asked ~= key then return toast("已换集或重新搜索，请重新手动匹配") end
            lookup("episodes", tostring(v[i].id))
        end,
    })
end)

mp.register_script_message("episodes", function(json, arg)
    local v = reply("episodes", json, arg)
    local key = asked
    if not v then return end
    if #v.list == 0 then return toast("这部没有分集信息") end
    local items = {}
    for i, e in ipairs(v.list) do items[i] = e.title end
    input.select({
        prompt = "选择对应的一集：",
        items = items,
        default_item = (v.at or 0) + 1,
        submit = function(i)
            if asked ~= key then return toast("已换集或重新搜索，请重新手动匹配") end
            mp.commandv("script-message", "embymini-danmaku-pick", tostring(v.anime), tostring(i - 1))
        end,
    })
end)

-- Messages from EmbyMini ------------------------------------------------------------------------------------
-- init {style, words}: settings, when the first episode starts and when blocked words change
mp.register_script_message("init", function(json)
    local v = utils.parse_json(json) or {}
    for k, val in pairs(v.style or {}) do
        if DEFAULT[k] ~= nil then style[k] = val end
    end
    words = {}
    for _, w in ipairs(v.words or {}) do words[#words + 1] = w:lower() end
    announce()
    after("rebuild")
    publish()
    draw_panel()
end)

-- status {kind, text, toast, item, start}: loading, no match, error, no address. start = the episode's first
-- message (remember its item)
mp.register_script_message("status", function(json)
    local v = utils.parse_json(json) or {}
    if v.start then want = v.item elseif v.item ~= want then return end
    src = { kind = v.kind or "error", text = v.text or "" }
    if v.kind ~= "loading" then raw = nil; rebuild(); refresh() end -- while reloading the old ones stay
    if v.toast then toast(src.text) end
    publish()
    draw_panel()
end)

-- load {item, title, manual, stale, list: [[seconds, mode, colour, text, width (em)], …]}: this episode's danmaku
mp.register_script_message("load", function(json)
    local t0 = mp.get_time()
    local v = utils.parse_json(json)
    if not v or v.item ~= want then return end
    raw = {}
    for _, c in ipairs(v.list or {}) do
        raw[#raw + 1] = { t = c[1], mode = c[2], color = c[3], text = c[4], w = c[5] or em_width(c[4]) }
    end
    local note = (v.manual and "（手动匹配）" or "") .. (v.stale and "（接口暂不可用，用的旧缓存）" or "")
    src = { kind = "ok", text = v.title .. note, title = v.title, note = note }
    rebuild()
    mp.msg.verbose(string.format("载入 %d 条（显示 %d 条），%.0f ms", #raw, #list, (mp.get_time() - t0) * 1000))
    publish()
    relayout(now())
    refresh()
    draw_panel()
end)

-- ModernZ's danmaku button and the d / D keys
mp.register_script_message("toggle", function()
    style.on = not style.on
    save()
    mp.osd_message(style.on and "弹幕：开" or "弹幕：关", 1.5)
    after("relayout")
    draw_panel()
end)
mp.register_script_message("panel", open_panel)

-- mpv state -------------------------------------------------------------------------------------------------
mp.register_event("start-file", function()
    raw, list, active, offset, want, asked = nil, {}, {}, 0, nil, nil
    src = { kind = "idle", text = "" }
    publish()
    close_panel()
    refresh()
end)

mp.register_event("playback-restart", function()
    resync()
    relayout(now())
    refresh()
end)

mp.observe_property("core-idle", "bool", function(_, idle)
    resync()
    clock.run = idle == false
    refresh()
end)

mp.observe_property("time-pos", "number", function(_, v) tpos = v end)

mp.observe_property("speed", "number", function(_, v)
    resync()
    clock.rate = v or 1
end)

mp.observe_property("display-fps", "number", function(_, v)
    fps = (v and v > 0) and v or 60
    refresh()
end)

mp.observe_property("osd-dimensions", "native", function(_, d)
    W, H = d and d.w or 0, d and d.h or 0
    relayout(now())
    refresh()
    draw_panel()
end)
