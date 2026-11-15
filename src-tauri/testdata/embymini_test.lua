-- Runs mpv/scripts/embymini.lua against a fake `mp` and checks the episode menu and the paused cursor.
-- Started by the player test `embymini_script` (script path in EMBYMINI_LUATEST_SRC); quits mpv with 0 on success.
local real = mp

local fake = { props = { ["cursor-autohide"] = "1000" }, handlers = {}, observers = {}, sent = {} }
local m = {}
function m.get_property(k) return fake.props[k] end
function m.set_property(k, v) fake.props[k] = v end
function m.commandv(...) fake.sent[#fake.sent + 1] = { ... } end
function m.register_script_message(name, fn) fake.handlers[name] = fn end
function m.observe_property(name, _, fn) fake.observers[name] = fn end
local input = { select = function(o) fake.select = o end }
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

local function run()
    -- No episode list yet: the menu stays closed
    fake.handlers.menu()
    check(fake.select == nil, "menu without episodes")

    -- The client's index is 0-based, Lua's 1-based, and the jump goes back 0-based
    fake.handlers.episodes('["E1","E2","E3"]', "1")
    fake.handlers.menu()
    check(fake.select and #fake.select.items == 3, "menu lists the episodes")
    check(fake.select.default_item == 2, "current episode preselected, got " .. tostring(fake.select.default_item))
    fake.select.submit(2)
    check(#fake.sent == 0, "picking the current episode sends nothing")
    fake.select.submit(3)
    local c = fake.sent[1] or {}
    check(c[1] == "script-message" and c[2] == "embymini-jump" and c[3] == "2", "jump index, got " .. table.concat(c, " "))

    -- Paused: the cursor stays; resumed: the value from before the pause (a personal config may have changed it)
    local pause = fake.observers.pause
    pause("pause", false)
    check(fake.props["cursor-autohide"] == "1000", "unpaused at start leaves the cursor alone")
    pause("pause", true)
    check(fake.props["cursor-autohide"] == "no", "paused keeps the cursor")
    pause("pause", true)
    pause("pause", false)
    check(fake.props["cursor-autohide"] == "1000", "resumed restores 1000, got " .. tostring(fake.props["cursor-autohide"]))
end

local ok, err = pcall(function()
    local chunk = assert(loadfile(os.getenv("EMBYMINI_LUATEST_SRC"), "t", env))
    chunk()
    run()
end)
if not ok then
    failures = failures + 1
    print("ERROR: " .. tostring(err))
end
print(failures == 0 and "embymini_test: ok" or ("embymini_test: " .. failures .. " failed"))
real.commandv("quit", failures == 0 and "0" or "1")
