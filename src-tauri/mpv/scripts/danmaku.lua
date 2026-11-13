-- EmbyMini 弹幕：排版、逐帧绘制、设置面板、手动匹配。数据由 EmbyMini 取好经 IPC 发来（消息见文件末尾）。
--
-- 为什么不用字幕轨：mpv 的字幕按当前视频帧的时间渲染，24 帧片源上滚动弹幕每秒只动 24 次，高刷屏上一顿一顿。
-- 这里用独立的 osd-overlay，按屏幕刷新率的定时器逐帧算位置；时间用墙上时钟外推、再慢慢向 time-pos 校准，
-- 不跟视频帧走。OSD 一变 mpv 就会在两帧之间重绘，所以滚动和屏幕刷新率一样顺。
--
-- 排版同常见视频网站：滚动弹幕按行分配，后一条追不上前一条才进同一行，没有空行就丢掉（不重叠）；
-- 顶部 / 底部弹幕居中停留 FIXED 秒。显示区域只限制滚动和顶部弹幕，底部弹幕始终从底边往上排。

local utils = require "mp.utils"
local input = require "mp.input"

local DEFAULT = {
    on = true, opacity = 0.8, area = 0.5, scale = 1, speed = 3, bold = true, border = "heavy", merge = true,
    noScroll = false, noTop = false, noBottom = false, noColor = false,
}
local SPEEDS = { 13, 10, 8, 6.5, 5 }           -- 速度档 1–5：滚动弹幕横穿屏幕的秒数
local SPEED_NAMES = { "极慢", "较慢", "适中", "较快", "极快" }
local AREAS = { { 0.25, "1/4" }, { 0.5, "半屏" }, { 0.75, "3/4" }, { 1, "满屏" } }
local BORDERS = { { "heavy", "重墨" }, { "outline", "描边" }, { "shadow", "投影" } }
local FIXED = 5                                 -- 顶部 / 底部弹幕停留秒数
local MERGE_WINDOW = 15                         -- 合并重复：这么多秒内的相同内容只显示第一条
local FONT = package.config:sub(1, 1) == "\\" and "Microsoft YaHei" or "sans-serif"
local ACCENT = "ECAE00"                         -- 强调色（ASS 是 BGR）

local style = {}
for k, v in pairs(DEFAULT) do style[k] = v end
local words = {}
local offset = 0                                -- 时间偏移（秒，正数 = 弹幕推后），只对当前集，换集归零

local raw = nil                                 -- 本集弹幕：{ t, mode, color, text, w(字宽，em) }
local list = {}                                 -- 按设置过滤、合并后的
local src = { kind = "idle", text = "" }        -- 面板里的状态行：kind = idle / loading / ok / none / error / setup

-- 屏幕与时钟 -------------------------------------------------------------------------------------------------
local W, H = 0, 0
local fps = 60
local clock = { pos = 0, wall = 0, rate = 1, run = false }

local function now()
    if clock.run then return clock.pos + (mp.get_time() - clock.wall) * clock.rate end
    return clock.pos
end

local function resync()
    clock.pos = mp.get_property_number("time-pos") or clock.pos
    clock.wall = mp.get_time()
end

-- 每帧向 time-pos 校准一点：time-pos 只在视频帧变化时更新（24 帧片源上是锯齿），直接跟着走就又一顿一顿了；
-- 差太多（拖动、卡顿后）直接对齐
local function calibrate()
    local tp = mp.get_property_number("time-pos")
    if not tp then return end
    local err = tp - now()
    if math.abs(err) > 0.5 then
        resync()
        return true
    end
    clock.pos = clock.pos + err * 0.02
end

-- 数据 -------------------------------------------------------------------------------------------------------
-- 字宽估算（单位 em）：没法逐条量（量一次要走一遍 libass），按字符类别估，中日韩全角 1、半角 0.55。
-- EmbyMini 发来的数据已经带了（同一算法，见 danmaku.rs 的 em_width），这里只给合并后的「×N」后缀用
local function em_width(s)
    local w = 0
    for ch in s:gmatch("[%z\1-\127\194-\244][\128-\191]*") do
        local b = ch:byte()
        if b < 128 then w = w + 0.55 elseif b < 0xE0 then w = w + 0.6 elseif b < 0xF0 then w = w + 1 else w = w + 1.1 end
    end
    return w
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

-- 按设置重建要显示的列表（开关屏蔽类型、合并重复、屏蔽词时调用）
local function rebuild()
    list = {}
    if not raw then return end
    local last = {}                             -- 合并重复：内容 → 第一次出现的那条
    for _, c in ipairs(raw) do
        if not blocked(c) then
            local first = style.merge and last[c.text]
            if first and c.t - first.t <= MERGE_WINDOW then
                first.n = first.n + 1
            else
                local item = { t = c.t, mode = c.mode, color = c.color, text = c.text, w = c.w, n = 1 }
                last[c.text] = item
                list[#list + 1] = item
            end
        end
    end
    for _, item in ipairs(list) do
        if item.n > 1 then
            item.shown = item.text .. " ×" .. item.n
            item.w = item.w + em_width(" ×" .. item.n)
        else
            item.shown = item.text
        end
        -- ASS 转义：花括号会被当成样式块，反斜杠会被当成转义
        item.shown = item.shown:gsub("\\", "＼"):gsub("{", "｛"):gsub("}", "｝")
    end
end

-- 排版 -------------------------------------------------------------------------------------------------------
local active = {}
local rows = { [1] = {}, [4] = {}, [5] = {} }   -- 每种类型每行最后一条
local nexti = 1
local last_t = 0

local function fs() return math.max(14, H * 0.032 * style.scale) end
local function row_h() return fs() * 1.2 end
local function duration() return SPEEDS[style.speed] or 8 end

local function first_index(t)                   -- 第一条 t >= 给定时间的下标（二分）
    local lo, hi = 1, #list + 1
    while lo < hi do
        local mid = math.floor((lo + hi) / 2)
        if list[mid].t < t then lo = mid + 1 else hi = mid end
    end
    return lo
end

-- 从时间 t 重新排：清空屏幕，把最近 D 秒内该出现的重新放上去（拖动进度、改设置后）
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
            -- 前一条尾巴已经进屏，且到左边之前追不上它
            if not last or ((t0 - last.t0) * last.v >= last.w + f and t0 + W / v >= last.t0 + D) then
                local a = { item = item, row = r, t0 = t0, w = w, v = v, ends = t0 + D }
                rows[1][r] = a
                active[#active + 1] = a
                return
            end
        end
    else
        local n = item.mode == 5 and nscroll or math.max(1, math.floor(H * 0.5 / row_h()))
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

-- 绘制 -------------------------------------------------------------------------------------------------------
local overlay = mp.create_osd_overlay("ass-events")
overlay.z = -10                                 -- 在控制栏下面

local function bgr(c)
    return string.format("%02X%02X%02X", c % 256, math.floor(c / 256) % 256, math.floor(c / 65536) % 256)
end

local function head()
    local f = fs()
    local a = string.format("%02X", math.floor((1 - style.opacity) * 255 + 0.5))
    -- 每种描边只用一层（描边或阴影）：每多一层，每帧要打包上传的位图就多一份
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
    if t < last_t - 0.3 then relayout(t) end    -- 时间往回走了（倒退、换集后的时间）
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
            local c = a.item.color
            local dark = (c % 256 + math.floor(c / 256) % 256 + math.floor(c / 65536)) < 120
            local pos
            if a.item.mode == 1 then
                -- 取整到像素：libass 按亚像素位置缓存位图，取整后每帧都命中缓存，不用重新光栅化
                pos = string.format("\\an7\\pos(%d,%d)", math.floor(W - (t - a.t0) * a.v + 0.5), math.floor((a.row - 1) * rh + 0.5))
            elseif a.item.mode == 5 then
                pos = string.format("\\an8\\pos(%d,%d)", math.floor(W / 2), math.floor((a.row - 1) * rh + 0.5))
            else
                pos = string.format("\\an2\\pos(%d,%d)", math.floor(W / 2), math.floor(bottom - (a.row - 1) * rh + 0.5))
            end
            out[#out + 1] = string.format("{%s%s\\1c&H%s&\\3c&H%s&}%s", pos, h, bgr(c), dark and "FFFFFF" or "000000",
                a.item.shown)
        end
    end
    active = keep
    overlay.res_x, overlay.res_y = W, H
    overlay.data = table.concat(out, "\n")
    overlay:update()
end

local timer = mp.add_periodic_timer(1 / 60, function()
    calibrate()
    draw(now())
end)
timer:kill()

-- 只在播放中、开着、有弹幕时逐帧画；其余时候画一次静止的
local function refresh()
    local run = clock.run and style.on and raw ~= nil and W > 0
    timer:kill()
    if run then
        timer.timeout = 1 / math.max(24, math.min(fps, 240))
        timer:resume()
    end
    draw(now())
end

-- 设置面板 ---------------------------------------------------------------------------------------------------
local panel = mp.create_osd_overlay("ass-events")
panel.z = 1000
local open = false
local hits = {}
local drag = nil
local mouse = { x = -1, y = -1 }
local autohide = nil
local box = nil                                 -- 面板外框 {x0, y0, x1, y1}

local function save()
    mp.commandv("script-message", "embymini-danmaku-style", utils.format_json(style))
    mp.commandv("script-message", "embymini-danmaku-on", style.on and "yes" or "no")
end

local function toast(text) mp.osd_message("弹幕：" .. text, 4) end

-- 当前状态写进 user-data（EmbyMini 的端到端测试读它，排查时也可以在控制台里看）
local function publish()
    mp.set_property_native("user-data/embymini/danmaku", {
        on = style.on, kind = src.kind, text = src.text, count = raw and #raw or 0, shown = #list,
    })
end

local function rr(x0, y0, x1, y1, r)
    return string.format(
        "m %.1f %.1f l %.1f %.1f b %.1f %.1f %.1f %.1f %.1f %.1f l %.1f %.1f b %.1f %.1f %.1f %.1f %.1f %.1f " ..
        "l %.1f %.1f b %.1f %.1f %.1f %.1f %.1f %.1f l %.1f %.1f b %.1f %.1f %.1f %.1f %.1f %.1f",
        x0 + r, y0, x1 - r, y0, x1, y0, x1, y0, x1, y0 + r, x1, y1 - r, x1, y1, x1, y1, x1 - r, y1,
        x0 + r, y1, x0, y1, x0, y1, x0, y1 - r, x0, y0 + r, x0, y0, x0, y0, x0 + r, y0)
end

local function shape(out, x0, y0, x1, y1, r, color, alpha)
    out[#out + 1] = string.format("{\\an7\\pos(0,0)\\bord0\\shad0\\1c&H%s&\\1a&H%s&\\p1}%s{\\p0}",
        color, alpha or "00", rr(x0, y0, x1, y1, r))
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

local function after(change)                    -- 改了设置：relayout = 影响排版，rebuild = 影响过滤
    if change == "rebuild" then rebuild() end
    if change then relayout(now()) end
    refresh()
end

local draw_panel

local function close_panel()
    if not open then return end
    open, drag = false, nil
    panel:remove()
    for _, k in ipairs({ "MBTN_LEFT", "MBTN_LEFT_DBL", "ESC" }) do mp.remove_key_binding("dm-panel-" .. k) end
    if autohide then mp.set_property("cursor-autohide", autohide); autohide = nil end
end

local manual_match

draw_panel = function()
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

    -- 一排等宽的选项，cur(值) 为真的高亮
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

    local function slider(frac, value_text, set)
        local a, b = cx0 + 6 * k, cx1 - 56 * k
        local cy = y + rh / 2
        shape(out, a, cy - 2 * k, b, cy + 2 * k, 2 * k, "FFFFFF", "B0")
        shape(out, a, cy - 2 * k, a + (b - a) * frac, cy + 2 * k, 2 * k, ACCENT)
        local kx = a + (b - a) * frac
        shape(out, kx - 7 * k, cy - 7 * k, kx + 7 * k, cy + 7 * k, 7 * k, "FFFFFF")
        text(out, cx1, cy, 6, fsz * 0.93, value_text, "C8C8C8")
        hit(a - 8 * k, y, b + 8 * k, y + rh, function(mx) set(math.max(0, math.min(1, (mx - a) / (b - a)))) end, true)
    end

    -- 第 1 行：标题、开关
    text(out, lx, y + rh / 2, 4, fsz * 1.1, "弹幕设置", "FFFFFF")
    seg({ { true, "开启" }, { false, "关闭" } }, function(v) return style.on == v end, function(v)
        style.on = v; save(); after(); draw_panel()
    end, cx1 - 120 * k, cx1)
    y = y + rh
    -- 第 2 行：来源 / 状态
    local status = src.text ~= "" and src.text or (style.on and "播放后自动匹配弹幕" or "弹幕已关闭")
    if src.kind == "ok" and raw then status = string.format("%s · %d 条", src.text, #raw) end
    local bad = src.kind == "error" or src.kind == "setup"
    text(out, lx, y + rh / 2, 4, fsz * 0.86, (status:gsub("{", "｛")), bad and "6B8BFF" or "A0A0A0")
    y = y + rh
    label("屏蔽类型")
    seg({ { "noScroll", "滚动" }, { "noTop", "顶部" }, { "noBottom", "底部" }, { "noColor", "彩色" } },
        function(key) return style[key] end, function(key)
            style[key] = not style[key]; save(); after("rebuild"); draw_panel()
        end)
    y = y + rh
    label("不透明度")
    slider((style.opacity - 0.1) / 0.9, string.format("%d%%", math.floor(style.opacity * 100 + 0.5)), function(f)
        style.opacity = math.floor((0.1 + f * 0.9) * 20 + 0.5) / 20; after(); draw_panel()
    end)
    y = y + rh
    label("显示区域")
    seg(AREAS, function(v) return math.abs(style.area - v) < 0.01 end, function(v)
        style.area = v; save(); after("relayout"); draw_panel()
    end)
    y = y + rh
    label("弹幕字号")
    slider((style.scale - 0.5) / 1.5, string.format("%d%%", math.floor(style.scale * 100 + 0.5)), function(f)
        style.scale = math.floor((0.5 + f * 1.5) * 20 + 0.5) / 20; after("relayout"); draw_panel()
    end)
    y = y + rh
    label("弹幕速度")
    local speeds = {}
    for i, n in ipairs(SPEED_NAMES) do speeds[i] = { i, n } end
    seg(speeds, function(v) return style.speed == v end, function(v)
        style.speed = v; save(); after("relayout"); draw_panel()
    end)
    y = y + rh
    label("描边")
    local bopts = {}
    for i, b in ipairs(BORDERS) do bopts[i] = b end
    bopts[#bopts + 1] = { "bold", "粗体" }
    seg(bopts, function(v) if v == "bold" then return style.bold end return style.border == v end, function(v)
        if v == "bold" then style.bold = not style.bold else style.border = v end
        save(); after(); draw_panel()
    end)
    y = y + rh
    label("时间偏移")
    local mid = (cx0 + cx1) / 2
    seg({ { -0.5, "−0.5 秒" }, { 0, string.format("%+.1f 秒", offset) }, { 0.5, "+0.5 秒" } },
        function() return false end, function(d)
            offset = d == 0 and 0 or offset + d; after("relayout"); draw_panel()
        end, cx0, mid + 60 * k)
    seg({ { "merge", "合并重复" } }, function() return style.merge end, function()
        style.merge = not style.merge; save(); after("rebuild"); draw_panel()
    end, mid + 66 * k, cx1)
    y = y + rh
    seg({ { "match", "手动匹配" }, { "reload", "重新加载" }, { "reset", "恢复默认" } }, function() return false end,
        function(v)
            if v == "match" then close_panel(); manual_match()
            elseif v == "reload" then mp.commandv("script-message", "embymini-danmaku-reload")
            else
                for key, val in pairs(DEFAULT) do style[key] = val end
                offset = 0; save(); after("rebuild"); draw_panel()
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
    mp.add_forced_key_binding("MBTN_LEFT_DBL", "dm-panel-MBTN_LEFT_DBL", function() end)
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

-- 手动匹配：输入关键词 → 选番剧 / 剧 → 选集，结果由 EmbyMini 记住（同一季之后的集按同样的偏移）
manual_match = function()
    local title = mp.get_property("media-title", "")
    local kw = title:match("^(.-) S%d+E%d+") or title
    input.get({
        prompt = "搜索弹幕（剧名 / 番剧名）：",
        default_text = kw,
        submit = function(s)
            input.terminate()
            if s ~= "" then
                toast("正在搜索…")
                mp.commandv("script-message", "embymini-danmaku-search", s)
            end
        end,
    })
end

local function reply(json)
    local v = utils.parse_json(json)
    if not v or v.error then
        toast(v and v.error or "搜索失败")
        return nil
    end
    return v
end

mp.register_script_message("search", function(json)
    local v = reply(json)
    if not v then return end
    if #v == 0 then return toast("没有搜到，换个关键词试试") end
    local items = {}
    for i, a in ipairs(v) do items[i] = string.format("%s（%d 集）", a.title, a.count) end
    mp.osd_message("")
    input.select({
        prompt = "选择剧集：",
        items = items,
        submit = function(i) mp.commandv("script-message", "embymini-danmaku-episodes", tostring(v[i].id)) end,
    })
end)

mp.register_script_message("episodes", function(json)
    local v = reply(json)
    if not v then return end
    if #v.list == 0 then return toast("这部没有分集信息") end
    local items = {}
    for i, e in ipairs(v.list) do items[i] = e.title end
    input.select({
        prompt = "选择对应的一集：",
        items = items,
        default_item = (v.at or 0) + 1,
        submit = function(i)
            mp.commandv("script-message", "embymini-danmaku-pick", tostring(v.anime), tostring(i - 1))
        end,
    })
end)

-- 与 EmbyMini 的消息 -----------------------------------------------------------------------------------------
-- init {style, words}：第一集开播时下发设置
mp.register_script_message("init", function(json)
    local v = utils.parse_json(json) or {}
    for k, val in pairs(v.style or {}) do
        if DEFAULT[k] ~= nil then style[k] = val end
    end
    words = {}
    for _, w in ipairs(v.words or {}) do words[#words + 1] = w:lower() end
    mp.commandv("script-message", "embymini-danmaku-on", style.on and "yes" or "no")
    after("rebuild")
end)

-- status {kind, text, toast}：加载中、没匹配上、出错、没填接口
mp.register_script_message("status", function(json)
    local v = utils.parse_json(json) or {}
    src = { kind = v.kind or "error", text = v.text or "" }
    if v.kind ~= "loading" then raw = nil; rebuild(); refresh() end
    if v.toast then toast(src.text) end
    publish()
    draw_panel()
end)

-- load {title, manual, stale, list: [[秒, 类型, 颜色, 文字, 字宽(em)], …]}：本集弹幕
mp.register_script_message("load", function(json)
    local t0 = mp.get_time()
    local v = utils.parse_json(json)
    if not v then return end
    raw = {}
    for _, c in ipairs(v.list or {}) do
        raw[#raw + 1] = { t = c[1], mode = c[2], color = c[3], text = c[4], w = c[5] or em_width(c[4]) }
    end
    src = { kind = "ok", text = v.title .. (v.manual and "（手动匹配）" or "") .. (v.stale and "（接口暂不可用，用的旧缓存）" or "") }
    rebuild()
    mp.msg.verbose(string.format("载入 %d 条（显示 %d 条），%.0f ms", #raw, #list, (mp.get_time() - t0) * 1000))
    publish()
    relayout(now())
    refresh()
    draw_panel()
end)

-- 控制栏的弹幕按钮（ModernZ）和快捷键
mp.register_script_message("toggle", function()
    style.on = not style.on
    save()
    mp.osd_message(style.on and "弹幕：开" or "弹幕：关", 1.5)
    after("relayout")
    draw_panel()
end)
mp.register_script_message("panel", open_panel)

-- mpv 状态 ---------------------------------------------------------------------------------------------------
mp.register_event("start-file", function()
    raw, list, active, offset = nil, {}, {}, 0
    src = { kind = "idle", text = "" }
    publish()
    close_panel()
    refresh()
end)

mp.register_event("seek", function() resync() end)
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
