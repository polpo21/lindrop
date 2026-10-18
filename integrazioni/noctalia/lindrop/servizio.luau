--!nonstrict
-- lindrop - background service. Owns the truth: is the user unit
-- lindrop-ricevi.service running? Publishes it as "status" every 2 s, runs
-- sends (publishing "send" with a live stage message), and acts on "command"
-- from the widget, the tile and the panel. A send is routed here, not run from
-- the panel, so closing the panel never aborts it.

local UNIT = "lindrop-ricevi.service"
local state = "unknown" -- off | starting | on | stopping | unknown
local busy = false
local send = { state = "idle", info = "" } -- idle | picking | sending | done | failed
local resumeReceive = false -- we stopped receiving to free the radio for a send
local sendStartMs = 0 -- watchdog: when the current send began
local SEND_TIMEOUT_MS = 170000

local function publish()
    noctalia.state.set("status", { state = state })
end

local function publishSend()
    noctalia.state.set("send", { state = send.state, info = send.info })
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

local function finishSend(ok, info)
    send = { state = if ok then "done" else "failed", info = info }
    publishSend()
    if ok then
        noctalia.notify("AirDrop", info)
    else
        noctalia.notifyError("AirDrop", info)
    end
    if resumeReceive then
        resumeReceive = false
        run("start", "starting")
    end
end

-- A lindrop output line (ANSI stripped) → a short Italian status, or nil.
local function friendly(line)
    if string.find(line, "Accendo AWDL", 1, true) then return "Accendo la radio…" end
    if string.find(line, "AWDL sul canale", 1, true) then return "Radio pronta…" end
    if string.find(line, "Cerco iPhone", 1, true) then return "Cerco l'iPhone…" end
    if string.find(line, "Conferma sull'iPhone", 1, true) then return "Trovato! Conferma sull'iPhone…" end
    local pct = string.match(line, "(%d+)%%")
    if pct ~= nil then return "Invio… " .. pct .. "%" end
    return nil
end

-- Run `lindrop invia`, streaming each line to a live status. It forces
-- exclusive mode: the Wi-Fi blinks off for the few seconds of the transfer.
-- A trailing sentinel echoes the real exit code once lindrop is done.
local function sendFiles(files)
    local function shq(s)
        return "'" .. string.gsub(tostring(s), "'", "'\\''") .. "'"
    end
    -- LINDROP_ATTESA_ALTRI_S=0: with one iPhone there is no second device to
    -- wait for, so send as soon as it is found.
    local cmd = "LINDROP_ATTESA_ALTRI_S=0 lindrop invia"
    for _, f in ipairs(files) do
        cmd = cmd .. " " .. shq(f)
    end
    cmd = cmd .. " 2>&1; echo __LINDROP_EXIT__ $?"

    sendStartMs = noctalia.nowMs()
    send = { state = "sending", info = "Preparo l'invio…" }
    publishSend()

    local sentLine = nil -- the "Inviato a «X»…" line
    local lastErr = nil

    local function onLine(raw)
        local line = string.gsub(raw, "\27%[[0-9;]*m", "") -- strip ANSI
        local code = string.match(line, "^__LINDROP_EXIT__ (%d+)")
        if code ~= nil then
            if code == "0" then
                finishSend(true, sentLine or "Inviato all'iPhone")
            else
                finishSend(false, lastErr or "Invio non riuscito")
            end
            return
        end
        if string.find(line, "Inviato a", 1, true) then
            sentLine = line
        elseif string.find(line, "rifiutato", 1, true) then
            lastErr = "Rifiutato sull'iPhone"
        elseif string.find(line, "nessun iPhone", 1, true) then
            lastErr = "Nessun iPhone trovato"
        elseif string.match(line, "^errore:") ~= nil then
            lastErr = string.gsub(line, "^errore:%s*", "")
        end
        if send.state == "sending" then
            local msg = friendly(line)
            if msg ~= nil then
                send.info = msg
                publishSend()
            end
        end
    end

    if type(noctalia.runStream) == "function" then
        if not noctalia.runStream(cmd, onLine) then
            finishSend(false, "Impossibile avviare l'invio")
        end
    else
        -- Fallback without live stages (older shells): one callback at the end.
        local argv = { "env", "LINDROP_ATTESA_ALTRI_S=0", "lindrop", "invia" }
        for _, f in ipairs(files) do
            argv[#argv + 1] = f
        end
        noctalia.runAsync(argv, function(s)
            local out = (s.stdout or "") .. "\n" .. (s.stderr or "")
            if s.exitCode == 0 then
                finishSend(true, string.match(out, "Inviato[^\n]*") or "Inviato all'iPhone")
            else
                finishSend(false, string.match(out, "([^\n]+)%s*$") or "Invio non riuscito")
            end
        end, 150000)
    end
end

-- Pick files with a GUI chooser; the radio serves one client at a time, so if
-- we are receiving, stop it first and resume it when the send is done. The
-- receiver is checked straight from systemctl, not the cached state, which is
-- stale right after a reload.
local function startSend()
    if send.state == "picking" or send.state == "sending" then return end
    send = { state = "picking", info = "" }
    publishSend()
    noctalia.runAsync(
        { "zenity", "--file-selection", "--multiple", "--separator=\n", "--title=Invia con AirDrop" },
        function(r)
            local files = {}
            for line in string.gmatch(r.stdout or "", "[^\n]+") do
                files[#files + 1] = line
            end
            if r.exitCode ~= 0 or #files == 0 then
                send = { state = "idle", info = "" } -- annullato
                publishSend()
                return
            end
            noctalia.runAsync({ "systemctl", "--user", "is-active", UNIT }, function(a)
                local act = string.gsub(a.stdout or "", "%s+", "")
                if act ~= "active" and act ~= "activating" then
                    sendFiles(files)
                    return
                end
                -- Free the radio: stop the receiver, let it release the socket,
                -- then send.
                resumeReceive = true
                busy = true
                state = "stopping"
                sendStartMs = noctalia.nowMs()
                send = { state = "sending", info = "Spengo la ricezione…" }
                publish()
                publishSend()
                noctalia.runAsync({ "systemctl", "--user", "stop", UNIT }, function()
                    state = "off"
                    publish()
                    send.info = "Libero la radio…"
                    publishSend()
                    noctalia.runAsync({ "sh", "-c", "sleep 0.5" }, function()
                        busy = false
                        sendFiles(files)
                    end, 4000)
                end, 15000)
            end, 5000)
        end, 120000)
end

local function onCommand(cmd)
    if type(cmd) ~= "table" then return end
    if cmd.action == "toggle" then
        if state == "on" or state == "starting" then run("stop", "stopping") else run("start", "starting") end
    elseif cmd.action == "start" then
        run("start", "starting")
    elseif cmd.action == "stop" then
        run("stop", "stopping")
    elseif cmd.action == "invia" then
        startSend()
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
    -- Watchdog: never leave a send stuck (or the receiver off) if lindrop dies
    -- without its sentinel line.
    if send.state == "sending" and sendStartMs > 0 and noctalia.nowMs() - sendStartMs > SEND_TIMEOUT_MS then
        finishSend(false, "Invio scaduto")
    end
    poll()
end

noctalia.state.watch("command", onCommand)
publish()
publishSend()
poll()
