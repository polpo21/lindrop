--!nonstrict
-- lindrop - background service. Owns the truth: is the user unit
-- lindrop-ricevi.service running? Publishes "status" (receive), "send" (a send
-- in progress, with a live stage) and "devices" (the "send to…" menu). Acts on
-- "command" from the widget, the tile and the panel. Sends and scans are run
-- here, not from the panel, so closing it never aborts them.

local UNIT = "lindrop-ricevi.service"
local CACHE = "\"$HOME/.cache/lindrop/dispositivi.txt\""

local state = "unknown" -- off | starting | on | stopping | unknown
local busy = false
local send = { state = "idle", info = "" } -- idle | picking | sending | done | failed
local resumeReceive = false -- we stopped receiving to free the radio
local sendStartMs = 0
local scanStartMs = 0
local SEND_TIMEOUT_MS = 170000
local SCAN_TIMEOUT_MS = 40000

local devices = {} -- known receiver names, for the panel's menu
local scanning = false

local function shq(s)
    return "'" .. string.gsub(tostring(s), "'", "'\\''") .. "'"
end

local function publish()
    noctalia.state.set("status", { state = state })
end

local function publishSend()
    noctalia.state.set("send", { state = send.state, info = send.info })
end

local function publishDevices()
    noctalia.state.set("devices", { list = devices, scanning = scanning })
end

local function hasDevice(name)
    for _, n in ipairs(devices) do
        if n == name then return true end
    end
    return false
end

local function addDevice(name)
    local n = string.gsub(tostring(name), "%s+$", "")
    n = string.gsub(n, "^%s+", "")
    if n ~= "" and not hasDevice(n) then
        devices[#devices + 1] = n
        return true
    end
    return false
end

local function saveDevices()
    local script = "mkdir -p \"$HOME/.cache/lindrop\" && : > " .. CACHE
    if #devices > 0 then
        local args = ""
        for _, n in ipairs(devices) do
            args = args .. " " .. shq(n)
        end
        script = "mkdir -p \"$HOME/.cache/lindrop\" && printf '%s\\n'" .. args .. " > " .. CACHE
    end
    noctalia.runAsync({ "sh", "-c", script })
end

local function loadDevices()
    noctalia.runAsync({ "sh", "-c", "cat " .. CACHE .. " 2>/dev/null" }, function(r)
        for line in string.gmatch(r.stdout or "", "[^\n]+") do
            addDevice(line)
        end
        publishDevices()
    end, 4000)
end

local function poll()
    if busy then return end
    noctalia.runAsync({ "systemctl", "--user", "is-active", UNIT }, function(r)
        local out = string.gsub(r.stdout or "", "%s+", "")
        local new = if out == "active" then "on"
            elseif out == "activating" then "starting"
            elseif out == "deactivating" then "stopping"
            else "off"
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

local function releaseRadio()
    if resumeReceive then
        resumeReceive = false
        run("start", "starting")
    end
end

-- The radio serves one client at a time. If we are receiving, stop it, let it
-- release the socket, then call onReady; `note` reports the stage meanwhile.
local function freeRadio(note, onReady)
    noctalia.runAsync({ "systemctl", "--user", "is-active", UNIT }, function(a)
        local act = string.gsub(a.stdout or "", "%s+", "")
        if act ~= "active" and act ~= "activating" then
            onReady()
            return
        end
        resumeReceive = true
        busy = true
        state = "stopping"
        publish()
        note("Spengo la ricezione…")
        noctalia.runAsync({ "systemctl", "--user", "stop", UNIT }, function()
            state = "off"
            publish()
            note("Libero la radio…")
            noctalia.runAsync({ "sh", "-c", "sleep 0.5" }, function()
                busy = false
                onReady()
            end, 4000)
        end, 15000)
    end, 5000)
end

local function finishSend(ok, info)
    send = { state = if ok then "done" else "failed", info = info }
    publishSend()
    if ok then
        noctalia.notify("AirDrop", info)
        local who = string.match(info, "«(.-)»")
        if who ~= nil and addDevice(who) then
            saveDevices()
            publishDevices()
        end
    else
        noctalia.notifyError("AirDrop", info)
    end
    releaseRadio()
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

-- Run `lindrop invia` (optionally `-a target`), streaming each line to a live
-- status. A trailing sentinel echoes the real exit code once lindrop is done.
local function sendFiles(files, target)
    local cmd = "LINDROP_ATTESA_ALTRI_S=0 lindrop invia"
    if target ~= nil and target ~= "" then
        cmd = cmd .. " -a " .. shq(target)
    end
    for _, f in ipairs(files) do
        cmd = cmd .. " " .. shq(f)
    end
    cmd = cmd .. " 2>&1; echo __LINDROP_EXIT__ $?"

    sendStartMs = noctalia.nowMs()
    send = { state = "sending", info = if target ~= "" then ("Invio a «" .. target .. "»…") else "Preparo l'invio…" }
    publishSend()

    local sentLine = nil
    local lastErr = nil

    local function onLine(raw)
        local line = string.gsub(raw, "\27%[[0-9;]*m", "")
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
        elseif string.match(line, "^errore:") ~= nil then
            -- lindrop stampa ogni errore con questo prefisso (anche "nessun
            -- iPhone trovato" e "nessun «X» tra …"), quindi qui li prendo tutti.
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
        local argv = { "env", "LINDROP_ATTESA_ALTRI_S=0", "lindrop", "invia" }
        if target ~= nil and target ~= "" then
            argv[#argv + 1] = "-a"
            argv[#argv + 1] = target
        end
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

-- Pick files with a GUI chooser, free the radio if needed, then send.
local function startSend(target)
    -- La radio serve un client alla volta: non partire se una scansione è in
    -- corso (refreshDevices fa la guardia simmetrica).
    if send.state == "picking" or send.state == "sending" or scanning then return end
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
            sendStartMs = noctalia.nowMs()
            send = { state = "sending", info = "Preparo l'invio…" }
            publishSend()
            freeRadio(function(m)
                send.info = m
                publishSend()
            end, function()
                sendFiles(files, target)
            end)
        end, 120000)
end

-- Scan for nearby devices to fill the "send to…" menu.
local function finishScan()
    scanning = false
    saveDevices()
    publishDevices()
    releaseRadio()
end

local function refreshDevices()
    if scanning or send.state == "sending" or send.state == "picking" then return end
    scanning = true
    scanStartMs = noctalia.nowMs()
    publishDevices()
    local avvisato = false
    freeRadio(function()
        -- freeRadio chiama la nota solo se stava davvero ricevendo: lo dico una
        -- volta, così la pausa della ricezione non è silenziosa (si riaccende da
        -- sola a fine scansione via resumeReceive).
        if not avvisato then
            avvisato = true
            noctalia.notify("AirDrop", "Metto in pausa la ricezione per cercare i dispositivi")
        end
    end, function()
        local cmd = "lindrop dispositivi 2>&1; echo __LINDROP_EXIT__ $?"
        local function onLine(raw)
            local line = string.gsub(raw, "\27%[[0-9;]*m", "")
            if string.match(line, "^__LINDROP_EXIT__ ") ~= nil then
                finishScan()
                return
            end
            local name = string.match(line, "^DISPOSITIVO:(.+)$")
            if name ~= nil and addDevice(name) then
                publishDevices()
            end
        end
        if type(noctalia.runStream) == "function" then
            if not noctalia.runStream(cmd, onLine) then finishScan() end
        else
            noctalia.runAsync({ "lindrop", "dispositivi" }, function(s)
                for line in string.gmatch((s.stdout or ""), "[^\n]+") do
                    local name = string.match(line, "^DISPOSITIVO:(.+)$")
                    if name ~= nil then addDevice(name) end
                end
                finishScan()
            end, 30000)
        end
    end)
end

local function radioBusy()
    return scanning or send.state == "sending" or send.state == "picking"
end

-- Accende/spegne la ricezione. Se la radio è occupata (invio o scansione), non
-- tocca il servizio adesso — la radio serve un client alla volta — ma registra
-- l'intento: releaseRadio() lo applica quando la radio si libera.
local function setReceive(on)
    if radioBusy() then
        resumeReceive = on
        noctalia.notify("AirDrop", if on then "Attivo la ricezione appena finisce" else "Spengo la ricezione appena finisce")
        return
    end
    if on then run("start", "starting") else run("stop", "stopping") end
end

local function onCommand(cmd)
    if type(cmd) ~= "table" then return end
    if cmd.action == "toggle" then
        setReceive(not (state == "on" or state == "starting"))
    elseif cmd.action == "start" then
        setReceive(true)
    elseif cmd.action == "stop" then
        setReceive(false)
    elseif cmd.action == "invia" then
        startSend(cmd.a or "")
    elseif cmd.action == "aggiorna" then
        refreshDevices()
    elseif cmd.action == "open" then
        noctalia.runAsync({ "lindrop", "apri" })
    end
end

-- IPC too: `noctalia msg plugin polpo/lindrop:service all toggle`
function onIpc(event, _payload)
    onCommand({ action = event })
end

noctalia.setUpdateInterval(2000)
function update()
    if send.state == "sending" and sendStartMs > 0 and noctalia.nowMs() - sendStartMs > SEND_TIMEOUT_MS then
        finishSend(false, "Invio scaduto")
    end
    if scanning and scanStartMs > 0 and noctalia.nowMs() - scanStartMs > SCAN_TIMEOUT_MS then
        finishScan()
    end
    poll()
end

noctalia.state.watch("command", onCommand)
publish()
publishSend()
publishDevices()
loadDevices()
poll()
