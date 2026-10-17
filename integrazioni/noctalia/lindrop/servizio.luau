--!nonstrict
-- lindrop - background service. Owns the only truth: is the user unit
-- lindrop-ricevi.service running? Publishes it as "status" every 2 s, and
-- starts/stops the unit when a widget or the tile writes to "command".

local UNIT = "lindrop-ricevi.service"
local state = "unknown" -- off | starting | on | stopping | unknown
local busy = false

local function publish()
    noctalia.state.set("status", { state = state })
end

local function poll()
    if busy then return end
    noctalia.runAsync({ "systemctl", "--user", "is-active", UNIT }, function(r)
        local out = string.gsub(r.stdout or "", "%s+", "")
        local new = if out == "active" then "on"
            elseif out == "activating" then "starting"
            elseif out == "deactivating" then "stopping"
            else "off"
        -- It stopped by itself (radio busy, card unsupported…): say why.
        if out == "failed" or (state == "starting" and new == "off") then
            noctalia.runAsync({ "journalctl", "--user", "-u", UNIT, "-n", "1", "-o", "cat", "--no-pager" }, function(j)
                local why = string.gsub(j.stdout or "", "%s+$", "")
                noctalia.notifyError("AirDrop non partito", if why ~= "" then why else "vedi journalctl --user -u " .. UNIT)
            end, 3000)
            if out == "failed" then
                noctalia.runAsync({ "systemctl", "--user", "reset-failed", UNIT })
            end
        end
        if new ~= state then
            state = new
            publish()
        end
    end, 3000)
end

local function run(verb, transient)
    busy = true
    state = transient
    publish()
    noctalia.runAsync({ "systemctl", "--user", verb, "--no-block", UNIT }, function(r)
        busy = false
        if r.exitCode ~= 0 then
            noctalia.notifyError("lindrop", "systemctl " .. verb .. ": " .. (r.stderr or ""))
        end
        poll()
    end, 10000)
end

local function onCommand(cmd)
    if type(cmd) ~= "table" then return end
    if cmd.action == "toggle" then
        if state == "on" or state == "starting" then run("stop", "stopping") else run("start", "starting") end
    elseif cmd.action == "start" then
        run("start", "starting")
    elseif cmd.action == "stop" then
        run("stop", "stopping")
    elseif cmd.action == "open" then
        -- lindrop knows the folder (config `cartella`, else XDG_DOWNLOAD_DIR).
        noctalia.runAsync({ "lindrop", "apri" })
    end
end

-- IPC too: `noctalia msg plugin polpo/lindrop:service all toggle`
function onIpc(event, _payload)
    onCommand({ action = event })
end

noctalia.setUpdateInterval(2000)
function update()
    poll()
end

noctalia.state.watch("command", onCommand)
publish()
poll()
