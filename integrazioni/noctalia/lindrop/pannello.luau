--!nonstrict
-- lindrop - control-center panel: receive toggle, pick who to send to, send a
-- file, open the received-files folder. A pure renderer over the service's
-- "status", "send" and "devices" state; actions go back through "command".

local status = { state = "unknown" }
local send = { state = "idle", info = "" }
local devs = { list = {}, scanning = false }
local sel = 0 -- 0 = Automatico, else index into devs.list

local function cmd(action, extra)
    local msg = { action = action, t = noctalia.nowMs() }
    if extra ~= nil then
        for k, v in pairs(extra) do msg[k] = v end
    end
    noctalia.state.set("command", msg)
end

local function chosenName()
    if sel <= 0 then return "" end
    return (devs.list or {})[sel] or ""
end

local function render()
    local st = status.state or "unknown"
    local on = st == "on" or st == "starting"
    local recv = if st == "on" then "In ricezione"
        elseif st == "starting" then "Accensione…"
        elseif st == "stopping" then "Spegnimento…"
        else "Spenta"

    local list = devs.list or {}
    if sel > #list then sel = 0 end
    local options = { "Automatico" }
    for _, n in ipairs(list) do
        options[#options + 1] = n
    end

    local busy = send.state == "picking" or send.state == "sending"
    local scanning = devs.scanning == true
    local sendMsg = if send.state == "picking" then "Scegli i file da inviare…"
        elseif send.state == "sending" then (send.info ~= "" and send.info or "Invio in corso…")
        elseif send.state == "done" then (send.info or "Inviato")
        elseif send.state == "failed" then (send.info or "Invio non riuscito")
        else ""

    local children = {
        ui.row({ gap = 10, align = "center" }, {
            ui.glyph({ name = if on then "access-point" else "access-point-off", size = 24 }),
            ui.column({ gap = 1, flexGrow = 1 }, {
                ui.label({ text = "AirDrop", fontSize = 16, fontWeight = "bold" }),
                ui.label({ text = recv, fontSize = 12, color = "on_surface_variant" }),
            }),
            ui.toggle({
                checked = on,
                enabled = st ~= "unknown",
                onChange = function(v) cmd(if v == "true" then "start" else "stop") end,
            }),
        }),
        ui.row({ gap = 8, align = "center" }, {
            ui.label({ text = "Manda a:", fontSize = 12 }),
            ui.select({
                options = options,
                selectedIndex = sel,
                enabled = not busy,
                onChange = function(v) sel = tonumber(v) or 0; render() end,
                flexGrow = 1,
            }),
            ui.button({
                glyph = "refresh",
                variant = "ghost",
                enabled = not scanning and not busy,
                tooltip = "Cerca i dispositivi vicini",
                onClick = function() cmd("aggiorna") end,
            }),
        }),
    }

    if scanning then
        table.insert(children, ui.label({ text = "Cerco dispositivi vicini…", fontSize = 11, color = "on_surface_variant" }))
    elseif #list == 0 then
        table.insert(children, ui.label({ text = "Premi ⟳ per cercare i dispositivi (o lascia Automatico).", fontSize = 11, color = "on_surface_variant", maxLines = 2 }))
    end

    table.insert(children, ui.button({
        text = if busy then "Invio in corso…" else "Invia file…",
        glyph = "send",
        variant = "primary",
        enabled = not busy and not scanning,
        onClick = function() cmd("invia", { a = chosenName() }) end,
    }))

    if sendMsg ~= "" then
        table.insert(children, ui.label({ text = sendMsg, fontSize = 12, color = "on_surface_variant", maxLines = 3 }))
    end

    table.insert(children, ui.spacer({ flexGrow = 1 }))
    table.insert(children, ui.button({
        text = "Apri cartella ricevuti",
        glyph = "folder",
        variant = "outline",
        onClick = function() cmd("open") end,
    }))

    panel.render(ui.column({ gap = 12, padding = 16, align = "stretch", flexGrow = 1 }, children))
end

function onOpen(_context)
    status = noctalia.state.get("status") or status
    send = noctalia.state.get("send") or send
    devs = noctalia.state.get("devices") or devs
    sel = 0
    render()
end

noctalia.state.watch("status", function(v)
    if type(v) == "table" then status = v; render() end
end)

noctalia.state.watch("send", function(v)
    if type(v) == "table" then send = v; render() end
end)

noctalia.state.watch("devices", function(v)
    if type(v) == "table" then devs = v; render() end
end)
